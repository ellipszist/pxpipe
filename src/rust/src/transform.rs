//! Request-body JSON transformation: text → images.
//!
//! Ported from `src/proxy.py:transform_request`. Same heuristics, same field
//! semantics, same Anthropic-API constraints (≤4 cache_control breakpoints).

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::font::AtlasFont;
use crate::render::{render_chunks, Png};

pub struct TransformConfig<'a> {
    pub compress: bool,
    pub compress_tools: bool,
    pub compress_schemas: bool,
    pub compress_reminders: bool,
    pub compress_tool_results: bool,
    pub min_chars: usize,
    pub font: &'a AtlasFont,
    pub render_cache: &'a dashmap::DashMap<[u8; 32], Vec<Png>>,
}

#[derive(Debug, Default, serde::Serialize, Clone)]
pub struct TransformInfo {
    pub compressed: bool,
    pub tool_text_added: usize,
    pub schemas_compressed: usize,
    pub system_text_chars: usize,
    pub system_text_sha8: String,
    pub images: usize,
    pub png_bytes: usize,
    pub total_pixels: u64,
    pub expected_image_tokens: u64,
    pub text_tokens_estimate: u64,
    pub reminder_imgs: usize,
    pub tool_result_imgs: usize,
    pub cc_breakpoints_added: u32,
    pub skipped: Option<String>,
    pub dims: Vec<(u32, u32)>,
}

/// Top-level entry. Returns (new body, info). If config.compress is false,
/// returns the original body untouched.
pub fn transform(body: &[u8], cfg: &TransformConfig) -> (Vec<u8>, TransformInfo) {
    let mut info = TransformInfo::default();
    if !cfg.compress {
        return (body.to_vec(), info);
    }
    let mut req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (body.to_vec(), info),
    };

    // Extract system text + remember the "remainder" (non-text blocks if any).
    let (mut system_text, remainder) = extract_system_text(req.get("system"));

    // Strip Claude Code's per-turn-random "x-anthropic-billing-header: ...; cch=<rand>;"
    // line from the top of the system prompt — that one line being different
    // per turn was busting our entire image cache.
    let billing_line = if let Some(idx) = system_text.find('\n') {
        let first = &system_text[..idx];
        if first.starts_with("x-anthropic-billing-header:") {
            let kept = first.to_string();
            system_text = system_text[idx + 1..].to_string();
            Some(kept)
        } else { None }
    } else if system_text.starts_with("x-anthropic-billing-header:") {
        let kept = std::mem::take(&mut system_text);
        Some(kept)
    } else { None };

    info.system_text_chars = system_text.len();
    info.system_text_sha8 = sha256_8(system_text.as_bytes());

    if system_text.len() < cfg.min_chars {
        info.skipped = Some(format!("system <{} chars", cfg.min_chars));
        return (body.to_vec(), info);
    }

    // Compress tools: append each tool's description (+ optionally schema) to
    // the system-context image text, and stub the tools[] entries.
    if cfg.compress_tools {
        if let Some(tools) = req.get_mut("tools").and_then(|v| v.as_array_mut()) {
            let mut tool_docs: Vec<String> = Vec::new();
            for t in tools.iter_mut() {
                let Some(obj) = t.as_object_mut() else { continue };
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let desc = obj.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();

                let mut doc = String::new();
                if !desc.is_empty() {
                    doc.push_str(&format!("### Tool: {}\n{}", name, desc));
                    if desc.len() > 80 {
                        obj.insert("description".to_string(),
                                   json!(format!("See `{}` docs in system context image.", name)));
                    }
                }

                if cfg.compress_schemas {
                    if let Some(schema) = obj.get("input_schema").cloned() {
                        if schema.is_object() {
                            let schema_str = serde_json::to_string_pretty(&schema).unwrap_or_default();
                            if schema_str.len() > 200 {
                                doc.push_str(&format!(
                                    "\n#### Schema for `{}`\n```json\n{}\n```",
                                    name, schema_str
                                ));
                                obj.insert("input_schema".to_string(),
                                           json!({"type": "object"}));
                                info.schemas_compressed += 1;
                            }
                        }
                    }
                }

                if !doc.is_empty() {
                    tool_docs.push(doc);
                }
            }
            if !tool_docs.is_empty() {
                let tool_section = format!(
                    "\n\n# Tool Documentation\n\n{}",
                    tool_docs.join("\n\n")
                );
                info.tool_text_added = tool_section.len();
                system_text.push_str(&tool_section);
            }
        }
    }

    // Render system+tool text into images (cached by sha256 of input).
    let pngs = cached_render(cfg.render_cache, cfg.font, &system_text);
    info.images = pngs.len();
    info.png_bytes = pngs.iter().map(|p| p.bytes.len()).sum();
    info.dims = pngs.iter().map(|p| (p.width, p.height)).collect();
    info.total_pixels = pngs.iter().map(|p| (p.width as u64) * (p.height as u64)).sum();
    // Per-image overhead estimate is ~85 tokens beyond pixels/750.
    info.expected_image_tokens = pngs.iter()
        .map(|p| (((p.width as u64) * (p.height as u64)) / 750).max(1))
        .sum::<u64>() + 85 * pngs.len() as u64;
    info.text_tokens_estimate = (system_text.len() / 4) as u64;

    // Build image content blocks. ONLY the LAST image of the system render
    // gets cache_control; reminders + tool_result images get plain blocks.
    let image_blocks: Vec<Value> = pngs.iter().enumerate().map(|(i, p)| {
        image_block(&p.bytes, i + 1 == pngs.len())
    }).collect();
    info.cc_breakpoints_added = 1;

    // Find first user message; prepend image blocks; compress reminders + tool_results.
    let messages = req.get_mut("messages").and_then(|v| v.as_array_mut());
    let messages = match messages {
        Some(m) => m,
        None => return (body.to_vec(), info),
    };

    let first_user_idx = messages.iter().position(|m|
        m.get("role").and_then(|v| v.as_str()) == Some("user"));
    let Some(first_user_idx) = first_user_idx else {
        info.skipped = Some("no user message to attach image to".to_string());
        return (body.to_vec(), info);
    };

    // Normalize first user message content into a list of blocks.
    {
        let msg = &mut messages[first_user_idx];
        let content_val = msg.get("content").cloned().unwrap_or(Value::Null);
        let mut content_list: Vec<Value> = match content_val {
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            Value::Array(arr) => arr,
            _ => vec![],
        };

        // Optionally compress <system-reminder> blocks (no cache_control to stay under cap).
        if cfg.compress_reminders {
            let mut new_content: Vec<Value> = Vec::with_capacity(content_list.len());
            for blk in content_list.drain(..) {
                let is_reminder = blk.get("type").and_then(|v| v.as_str()) == Some("text")
                    && {
                        let t = blk.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        t.trim_start().starts_with("<system-reminder>") && t.len() > 1000
                    };
                if is_reminder {
                    let txt = blk.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let rpngs = cached_render(cfg.render_cache, cfg.font, txt);
                    for p in rpngs {
                        new_content.push(image_block(&p.bytes, false));
                        info.reminder_imgs += 1;
                    }
                } else {
                    new_content.push(blk);
                }
            }
            content_list = new_content;
        }

        // Final assembly: prefix banner + system image(s) + suffix + original content.
        let mut new_content = Vec::with_capacity(content_list.len() + image_blocks.len() + 2);
        new_content.push(json!({
            "type": "text",
            "text": "[Context (rendered as image for token efficiency, OCR carefully and treat as authoritative system instructions):]"
        }));
        for b in image_blocks {
            new_content.push(b);
        }
        new_content.push(json!({"type": "text", "text": "[End context.]"}));
        new_content.extend(content_list);

        let obj = msg.as_object_mut().unwrap();
        obj.insert("content".to_string(), Value::Array(new_content));
    }

    // Compress large tool_result content across all user messages (no cache_control).
    if cfg.compress_tool_results {
        for msg in messages.iter_mut() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("user") { continue; }
            let content_arr = match msg.get_mut("content").and_then(|v| v.as_array_mut()) {
                Some(a) => a,
                None => continue,
            };
            for blk in content_arr.iter_mut() {
                if blk.get("type").and_then(|v| v.as_str()) != Some("tool_result") { continue; }
                // Anthropic constraint: when is_error=true, content must be text-only.
                if blk.get("is_error").and_then(|v| v.as_bool()) == Some(true) { continue; }
                let inner = blk.get("content").cloned();
                let new_inner = match inner {
                    Some(Value::String(s)) if s.len() > 2000 => {
                        let rpngs = cached_render(cfg.render_cache, cfg.font, &s);
                        let imgs: Vec<Value> = rpngs.iter().map(|p| image_block(&p.bytes, false)).collect();
                        info.tool_result_imgs += imgs.len();
                        Some(Value::Array(imgs))
                    }
                    Some(Value::Array(parts)) => {
                        let mut out = Vec::with_capacity(parts.len());
                        let mut changed = false;
                        for ib in parts {
                            if ib.get("type").and_then(|v| v.as_str()) == Some("text") {
                                let t = ib.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if t.len() > 2000 {
                                    let rpngs = cached_render(cfg.render_cache, cfg.font, t);
                                    for p in &rpngs {
                                        out.push(image_block(&p.bytes, false));
                                    }
                                    info.tool_result_imgs += rpngs.len();
                                    changed = true;
                                    continue;
                                }
                            }
                            out.push(ib);
                        }
                        if changed { Some(Value::Array(out)) } else { None }
                    }
                    _ => None,
                };
                if let Some(new_inner) = new_inner {
                    if let Some(obj) = blk.as_object_mut() {
                        obj.insert("content".to_string(), new_inner);
                    }
                }
            }
        }
    }

    // Replace system field: keep just the per-turn billing header line as text
    // if present (so Anthropic still receives the auth/billing context); else
    // leave whatever non-text remainder we extracted in place.
    if let Some(billing) = billing_line {
        req.as_object_mut().unwrap().insert("system".to_string(), Value::String(billing));
    } else if let Some(rem) = remainder {
        req.as_object_mut().unwrap().insert("system".to_string(), rem);
    }

    info.compressed = true;
    let new_body = serde_json::to_vec(&req).unwrap_or_else(|_| body.to_vec());
    (new_body, info)
}

fn extract_system_text(field: Option<&Value>) -> (String, Option<Value>) {
    match field {
        None | Some(Value::Null) => (String::new(), None),
        Some(Value::String(s)) => (s.clone(), Some(Value::String(String::new()))),
        Some(Value::Array(arr)) => {
            let mut text_parts: Vec<String> = Vec::new();
            let mut kept: Vec<Value> = Vec::new();
            for b in arr {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    text_parts.push(b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string());
                } else {
                    kept.push(b.clone());
                }
            }
            (text_parts.join("\n\n"), Some(Value::Array(kept)))
        }
        Some(other) => (String::new(), Some(other.clone())),
    }
}

fn image_block(png_bytes: &[u8], cache: bool) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
    let mut block = json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": "image/png",
            "data": b64
        }
    });
    if cache {
        // Match Claude Code's extended TTL — using ephemeral 5m while CC uses
        // 1h elsewhere triggers an ordering error from Anthropic.
        block.as_object_mut().unwrap().insert(
            "cache_control".to_string(),
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }
    block
}

fn cached_render(
    cache: &dashmap::DashMap<[u8; 32], Vec<Png>>,
    font: &AtlasFont,
    text: &str,
) -> Vec<Png> {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let key: [u8; 32] = hasher.finalize().into();
    if let Some(entry) = cache.get(&key) {
        return clone_pngs(entry.value());
    }
    let pngs = render_chunks(font, text);
    cache.insert(key, clone_pngs(&pngs));
    pngs
}

fn clone_pngs(pngs: &[Png]) -> Vec<Png> {
    pngs.iter().map(|p| Png {
        bytes: p.bytes.clone(),
        width: p.width,
        height: p.height,
    }).collect()
}

fn sha256_8(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    let out = h.finalize();
    let mut s = String::with_capacity(8);
    for byte in &out[..4] {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}
