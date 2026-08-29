//! Optional local llama.cpp auto-tagger
//!
//! Talks to `llama-server`'s OpenAI-compatible endpoint. Repos are sent in
//! batches (default 10) as a single chat-completion request each, demanding
//! strict JSON back; results are merged into repo.json by the caller.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::LlmCfg;

/// What we send the model for one repo.
#[derive(Debug, Clone, Serialize)]
pub struct TagTarget {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Raw README excerpt: the richest source of tagging signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readme_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub existing_tags: Vec<String>,
}

/// What the model sends back for one repo.
#[derive(Debug, Clone, Deserialize)]
pub struct TagResult {
    pub name: String,
    pub tags: Vec<String>,
}

pub struct LlmTagger {
    url: String,
    model: Option<String>,
    batch_size: usize,
    disable_thinking: bool,
    client: reqwest::Client,
}

impl LlmTagger {
    pub fn new(cfg: &LlmCfg) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.request_timeout_secs.max(30)))
            .build()
            .unwrap_or_default();
        Self {
            url: cfg.url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            batch_size: cfg.batch_size.max(1),
            disable_thinking: cfg.disable_thinking,
            client,
        }
    }

    /// The request body additions shared by all chat calls.
    fn json_extras(&self, body: &mut serde_json::Value) {
        body["max_tokens"] = serde_json::json!(800); // hard cap: CPU servers are slow, and
        // a runaway thinking model must not generate forever
        if self.disable_thinking {
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
        }
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// One-shot chat completion returning the raw content string.
    /// Used by the import identifier (and reusable for future features).
    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.1,
            "response_format": {"type": "json_object"},
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::Value::String(m.clone());
        }
        self.json_extras(&mut body);
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.url))
            .json(&body)
            .send()
            .await
            .context("llm request failed (is llama-server running?)")?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.context("llm response was not JSON")?;
        if !status.is_success() {
            bail!("llm server returned {status}: {json}");
        }
        json["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("llm response missing choices[0].message.content"))
    }

    /// One repo per line, JSON-encoded; a batch of N repos per request.
    pub fn build_prompt(batch: &[TagTarget]) -> String {
        let repos: Vec<String> = batch.iter().map(|t| serde_json::to_string(t).unwrap()).collect();
        format!(
            "You are tagging git repositories for a personal archive. For each repository below, \
             produce 1 to 5 short, lowercase, single-word-or-kebab-case tags that categorize it \
             (language, platform, topic, project type). \
             The repository's existing tags take priority: they are the owner's choices; \
             do not suggest synonyms of them. \
             Base new tags primarily on the repository's README text (where the important \
             information lives), then on its name and description. Choose the aspects of the \
             project that seem most important. Do not repeat existing tags. \
             Respond ONLY with a JSON object of the form \
             {{\"results\":[{{\"name\":\"<repo name>\",\"tags\":[\"...\"]}}]}} covering every \
             repository listed, with no other text.\n\nRepositories:\n{}",
            repos.join("\n")
        )
    }

    /// Tag one batch (one chat completion). Returns parsed results.
    pub async fn tag_batch(&self, batch: &[TagTarget]) -> Result<Vec<TagResult>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let prompt = Self::build_prompt(batch);
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.1,
            "response_format": {"type": "json_object"},
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::Value::String(m.clone());
        }
        self.json_extras(&mut body);
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.url))
            .json(&body)
            .send()
            .await
            .context("llm request failed (is llama-server running?)")?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.context("llm response was not JSON")?;
        if !status.is_success() {
            bail!("llm server returned {status}: {json}");
        }
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("llm response missing choices[0].message.content"))?;
        parse_llm_tags(content)
    }
}

/// Parse the model's JSON output; tolerant of markdown fences and of a
/// bare top-level array instead of {"results": [...]}.
pub fn parse_llm_tags(content: &str) -> Result<Vec<TagResult>> {
    let s = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let v: serde_json::Value = serde_json::from_str(s).context("llm output is not valid JSON")?;
    let arr = if v.is_object() {
        v["results"].as_array().cloned().unwrap_or_default()
    } else if v.is_array() {
        v.as_array().cloned().unwrap_or_default()
    } else {
        bail!("llm output is neither object nor array");
    };
    let mut out = Vec::new();
    for item in arr {
        if let (Some(name), Some(tags)) = (item["name"].as_str(), item["tags"].as_array()) {
            let tags: Vec<String> = tags
                .iter()
                .filter_map(|t| t.as_str())
                .map(|s| s.trim().trim_matches(',').to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            out.push(TagResult { name: name.to_string(), tags });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_object() {
        let c = r#"{"results":[{"name":"sm64","tags":["decomps","n64"]},{"name":"oot","tags":["Decomps","zelda"]}]}"#;
        let r = parse_llm_tags(c).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "sm64");
        assert_eq!(r[0].tags, vec!["decomps", "n64"]);
        assert_eq!(r[1].tags, vec!["decomps", "zelda"]); // lowercased
    }

    #[test]
    fn parses_bare_array_and_fences() {
        let c = "```json\n[{\"name\":\"x\",\"tags\":[\"y\"]}]\n```";
        let r = parse_llm_tags(c).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].tags, vec!["y"]);
    }

    #[test]
    fn prompt_lists_every_repo() {
        let batch = vec![
            TagTarget { name: "a".into(), description: Some("d".into()), notes: None, readme_excerpt: Some("# A\nrust cli".into()), existing_tags: vec![] },
            TagTarget { name: "b".into(), description: None, notes: None, readme_excerpt: None, existing_tags: vec![] },
        ];
        let p = LlmTagger::build_prompt(&batch);
        assert!(p.contains("\"name\":\"a\""));
        assert!(p.contains("\"name\":\"b\""));
        assert!(p.contains("JSON object"));
        assert!(p.contains("existing tags take priority"));
        assert!(p.contains("README"));
    }

    #[test]
    fn garbage_fails_cleanly() {
        assert!(parse_llm_tags("I like turtles").is_err());
    }
}
/// Result of asking the LLM to identify an unknown zip.
#[derive(Debug, Clone, Default)]
pub struct IdentifyResult {
    pub name: Option<String>,
    pub queries: Vec<String>,
}

/// Parse the LLM's identify output: {"name": "...", "queries": [...]}
/// Tolerant of markdown fences and missing fields.
pub fn parse_llm_identify(content: &str) -> IdentifyResult {
    let s = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let mut out = IdentifyResult::default();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        if let Some(n) = v["name"].as_str() {
            out.name = Some(n.trim().to_string()).filter(|n| !n.is_empty());
        }
        if let Some(qs) = v["queries"].as_array() {
            out.queries = qs
                .iter()
                .filter_map(|q| q.as_str())
                .map(|q| q.trim().to_string())
                .filter(|q| !q.is_empty())
                .collect();
        }
    }
    out
}

#[cfg(test)]
mod identify_tests {
    use super::*;

    #[test]
    fn parses_identify_output() {
        let c = r#"{"name":"Super Mario 64", "queries":["sm64 decompilation", "n64 decomp mario"]}"#;
        let r = parse_llm_identify(c);
        assert_eq!(r.name.as_deref(), Some("Super Mario 64"));
        assert_eq!(r.queries.len(), 2);
    }

    #[test]
    fn tolerant_of_fences_and_missing_fields() {
        let r = parse_llm_identify("```json\n{\"queries\":[\"x\"]}\n```");
        assert!(r.name.is_none());
        assert_eq!(r.queries, vec!["x"]);
        let r = parse_llm_identify("garbage");
        assert!(r.name.is_none() && r.queries.is_empty());
    }
}
