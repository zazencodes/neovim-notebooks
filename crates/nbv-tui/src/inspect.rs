//! Full output inspection and explicit, session-owned external artifacts.
use anyhow::{Context, Result, bail};
use nbv_core::{Cell, document::multiline};
use serde_json::Value;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};

fn text(value: &Value) -> String {
    if value.is_string() || value.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
        multiline(Some(value))
    } else {
        serde_json::to_string_pretty(value).expect("JSON value")
    }
}

fn mime_text(value: &Value, mime: &str) -> String {
    if mime == "application/json" { serde_json::to_string_pretty(value).expect("JSON value") } else { text(value) }
}

/// Prefer the kernel's text table representation, then a data-resource table, then JSON.
fn representation(data: &Value, mime: Option<&str>) -> Result<String> {
    if let Some(mime) = mime {
        return data
            .get(mime)
            .map(|v| mime_text(v, mime))
            .with_context(|| format!("output has no {mime} representation"));
    }
    if let Some(value) = data.get("text/plain") {
        return Ok(text(value));
    }
    if let Some(table) = data.get("application/vnd.dataresource+json") {
        let fields = table.pointer("/schema/fields").and_then(Value::as_array).context("table has no schema fields")?;
        let names: Vec<&str> = fields
            .iter()
            .map(|f| f.get("name").and_then(Value::as_str).context("table field has no name"))
            .collect::<Result<_>>()?;
        let rows = table.get("data").and_then(Value::as_array).context("table has no data rows")?;
        let mut lines = vec![names.join("\t")];
        for row in rows {
            lines.push(
                names
                    .iter()
                    .map(|name| {
                        row.get(*name)
                            .map(|v| if let Some(s) = v.as_str() { s.to_owned() } else { v.to_string() })
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
        }
        return Ok(lines.join("\n"));
    }
    if let Some(value) = data.get("application/json") {
        return Ok(mime_text(value, "application/json"));
    }
    let mimes = data.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
    Ok(format!(
        "[no text preview; MIME: {mimes}]\nUse :NvbOutput <mime> for source or :NvbOpenOutput <output-number> <mime>."
    ))
}

/// A snapshot of every output, without inline truncation or terminal wrapping.
pub fn lines(cell: &Cell, mime: Option<&str>) -> Result<Vec<String>> {
    let mut lines = vec![];
    for (index, output) in cell.outputs().iter().enumerate() {
        let mimes =
            output.get("data").and_then(Value::as_object).map(|d| d.keys().cloned().collect::<Vec<_>>().join(", "));
        lines.push(match mimes {
            Some(mimes) => format!("--- output {} · MIME: {mimes} ---", index + 1),
            None => format!("--- output {} ---", index + 1),
        });
        let content = match output.get("output_type").and_then(Value::as_str) {
            Some("stream") => multiline(output.get("text")),
            Some("error") => output
                .get("traceback")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .map(|a| a.iter().map(text).collect::<Vec<_>>().join("\n"))
                .unwrap_or_else(|| {
                    format!(
                        "{}: {}",
                        output["ename"].as_str().unwrap_or("Error"),
                        output["evalue"].as_str().unwrap_or("")
                    )
                }),
            Some("execute_result" | "display_data") => {
                representation(&output["data"], mime).with_context(|| format!("output {}", index + 1))?
            }
            other => format!("[unsupported output: {other:?}]"),
        };
        // Neovim lines cannot contain embedded NUL bytes.
        let content = if matches!(output.get("output_type").and_then(Value::as_str), Some("stream" | "error")) {
            crate::ansi::lines(&content, ratatui::style::Style::default())
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            content
        };
        lines.extend(content.replace('\0', "␀").split('\n').map(str::to_owned));
    }
    if let Some(error) = &cell.runtime.error {
        lines.extend(error.lines().map(str::to_owned));
    }
    if lines.is_empty() {
        lines.push("[no output]".into());
    }
    Ok(lines)
}

pub struct Artifacts {
    dir: tempfile::TempDir,
}
impl Artifacts {
    pub fn new() -> Result<Self> {
        Ok(Self { dir: tempfile::Builder::new().prefix("nvb-output-").tempdir()? })
    }
    pub fn extract(&self, cell: &Cell, index: usize, mime: &str) -> Result<PathBuf> {
        let ext = match mime {
            "text/html" => "html",
            "image/svg+xml" => "svg",
            "text/plain" => "txt",
            "text/csv" => "csv",
            "application/json" => "json",
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            _ => bail!(
                "unsupported external MIME {mime}; supported: text/html, image/svg+xml, text/plain, text/csv, application/json, image/png, image/jpeg, image/gif"
            ),
        };
        let output = index
            .checked_sub(1)
            .and_then(|i| cell.outputs().get(i))
            .context("output number is outside this cell (numbers start at 1)")?;
        let value = output
            .get("data")
            .and_then(|d| d.get(mime))
            .with_context(|| format!("output {index} has no {mime} representation"))?;
        let bytes = if matches!(mime, "image/png" | "image/jpeg" | "image/gif") {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(text(value).chars().filter(|c| !c.is_whitespace()).collect::<String>())
                .context("invalid image base64")?
        } else {
            mime_text(value, mime).into_bytes()
        };
        let mut file =
            tempfile::Builder::new().prefix("output-").suffix(&format!(".{ext}")).tempfile_in(self.dir.path())?;
        file.write_all(&bytes)?;
        Ok(file.keep()?.1)
    }
    pub async fn open(&self, path: &Path) -> Result<()> {
        let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        open_with(program, path).await
    }
}
async fn open_with(program: &str, path: &Path) -> Result<()> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(program).arg(path).stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .context("external opener timed out after 10 seconds")?
    .with_context(|| format!("cannot start {program}"))?;
    if !result.status.success() {
        bail!("{program} failed ({}): {}", result.status, String::from_utf8_lossy(&result.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn cell(outputs: Value) -> Cell {
        Cell::from_raw(
            json!({"cell_type":"code", "metadata":{}, "source":"", "outputs":outputs, "execution_count":null})
                .as_object()
                .unwrap()
                .clone(),
        )
    }
    #[test]
    fn full_text_and_tables() {
        let c = cell(
            json!([{"output_type":"stream", "text":(0..150).map(|i| format!("row{i}\n")).collect::<String>()}, {"output_type":"display_data", "data":{"application/vnd.dataresource+json":{"schema":{"fields":[{"name":"name"},{"name":"count"}]},"data":[{"name":"alice","count":7}]}}}]),
        );
        let result = lines(&c, None).unwrap();
        assert!(result.contains(&"row0".into()));
        assert!(result.contains(&"row149".into()));
        assert!(result.contains(&"alice\t7".into()));
        assert!(lines(&c, Some("text/html")).is_err());
    }
    #[test]
    fn json_arrays_and_rich_only_mimes_are_preserved() {
        let c = cell(
            json!([{"output_type":"display_data", "data":{"application/json":["first", "second"]}}, {"output_type":"display_data", "data":{"text/html":"<table>hello</table>"}}]),
        );
        let result = lines(&c, None).unwrap().join("\n");
        assert!(result.contains("\"first\","));
        assert!(result.contains("MIME: text/html"));
        assert!(result.contains("no text preview"));
        let artifacts = Artifacts::new().unwrap();
        let path = artifacts.extract(&c, 1, "application/json").unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap(), json!(["first", "second"]));
    }
    #[tokio::test]
    async fn artifacts_are_exact_private_and_session_owned() {
        let c = cell(
            json!([{"output_type":"display_data", "data":{"text/html":["<table>","a</table>"], "image/svg+xml":"<svg/>"}}]),
        );
        let artifacts = Artifacts::new().unwrap();
        let html = artifacts.extract(&c, 1, "text/html").unwrap();
        assert_eq!(std::fs::read_to_string(&html).unwrap(), "<table>a</table>");
        let svg = artifacts.extract(&c, 1, "image/svg+xml").unwrap();
        assert_eq!(std::fs::read_to_string(&svg).unwrap(), "<svg/>");
        assert!(artifacts.extract(&c, 0, "text/html").is_err());
        assert!(artifacts.extract(&c, 1, "application/javascript").is_err());
        assert!(artifacts.extract(&c, 1, "image/png").is_err());
        assert!(open_with("/usr/bin/false", &html).await.unwrap_err().to_string().contains("failed"));
        assert!(open_with("/nvb-missing-opener", &html).await.unwrap_err().to_string().contains("cannot start"));
        drop(artifacts);
        assert!(!html.exists());
        assert!(!svg.exists());
    }
}
