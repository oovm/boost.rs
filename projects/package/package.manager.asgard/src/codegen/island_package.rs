//! Browser hydrate island packaging: auto glue + boot + CSS + manifest（无手写业务 JS）。
use crate::error::{AsgardError, AsgardResult};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

use crate::{
    awsl::LoweredComponent,
    codegen::{extract_and_merge_styles, generate_boot_script, generate_component_glue, manifest_url_for_mode},
    package::{ManifestDigest, ManifestUrlMode, generate_manifest_with_urls},
};

/// Options for packaging browser hydrate islands.
#[derive(Debug, Clone)]
pub struct IslandPackageOptions {
    /// Module stem used for CSS / manifest (`legion-test`, `asgard-app`, …).
    pub module_stem: String,
    /// WASM artifact stem (filename without extension).
    pub wasm_stem: String,
    /// Whether WASM artifacts were successfully built.
    pub wasm_built: bool,
    /// Use relative URLs in manifest (`c/foo.js`) instead of root-absolute (`/c/foo.js`).
    pub relative_urls: bool,
    /// CSS merge mode (`merge` / `scoped`, forwarded to extract_and_merge_styles).
    pub css_mode: String,
    /// Build mode string written into manifest.
    pub mode: String,
    /// Optional Tailwind CLI output appended to merged CSS.
    pub tailwind_css: Option<String>,
    /// 移植成本条目（`web_only` 块登记）；写入 manifest，非 dom 后端据此 fail-closed。
    pub porting_cost: Vec<String>,
}

impl Default for IslandPackageOptions {
    fn default() -> Self {
        Self {
            module_stem: "asgard-app".into(),
            wasm_stem: "asgard-app".into(),
            wasm_built: false,
            relative_urls: false,
            css_mode: "merge".into(),
            mode: "production".into(),
            tailwind_css: None,
            porting_cost: Vec::new(),
        }
    }
}

/// 计算已落盘 chunk 的 `sha256:<hex>` 摘要；文件缺失时返回 `None`（不伪造哈希）。
fn hash_wasm_chunk(output_dir: &Path, wasm_stem: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(output_dir.join(format!("{wasm_stem}.wasm"))).ok()?;
    let digest = Sha256::digest(&bytes);
    Some(format!("sha256:{}", digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()))
}

/// Report from [`package_browser_islands`].
#[derive(Debug, Clone)]
pub struct IslandPackageReport {
    /// Number of component glue JS files written.
    pub js_file_count: usize,
    /// CSS filenames written to `output_dir`.
    pub css_filenames: Vec<String>,
    /// Component route names packaged.
    pub routes: Vec<String>,
}

/// Write auto glue, boot.js, CSS, and manifest for hydrate islands.
///
/// Does **not** compile WASM — callers run `compile_wasm_bundle` first and pass `wasm_built`.
pub fn package_browser_islands(
    components: &[LoweredComponent],
    output_dir: &Path,
    opts: &IslandPackageOptions,
) -> AsgardResult<IslandPackageReport> {
    fs::create_dir_all(output_dir.join("c")).map_err(|e| AsgardError::msg(format!("{e}: {}", "创建 dist/c 失败")))?;

    let mut js_outputs = Vec::new();
    let mut routes = Vec::new();
    for component in components {
        let js = generate_component_glue(component, &opts.wasm_stem);
        let js_path = output_dir.join(&js.relative_path);
        if let Some(parent) = js_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&js_path, &js.content).map_err(|e| AsgardError::msg(format!("{e}: {}", "写入 JS 胶水失败")))?;
        routes.push(component.route_name.clone());
        js_outputs.push(js);
    }

    let css_outputs = extract_and_merge_styles(components, &opts.module_stem, &opts.css_mode, opts.tailwind_css.as_deref());
    let css_filenames: Vec<String> = css_outputs.iter().map(|css| css.filename.clone()).collect();
    for css in &css_outputs {
        let css_path = output_dir.join(&css.filename);
        fs::write(&css_path, &css.content).map_err(|e| AsgardError::msg(format!("{e}: {}", "写入 CSS 失败")))?;
    }

    let (provider_helpers, provider_bindings) = if opts.wasm_built {
        host_provider_js(output_dir, &opts.wasm_stem)?
    }
    else {
        (String::new(), String::new())
    };
    let boot_js = generate_boot_script(&opts.wasm_stem, manifest_url_for_mode(opts.relative_urls), &provider_helpers, &provider_bindings);
    fs::write(output_dir.join("boot.js"), &boot_js).map_err(|e| AsgardError::msg(format!("{e}: {}", "写入 boot.js 失败")))?;
    let _ = fs::remove_file(output_dir.join("asgard-runtime.js"));

    // 摘要必须来自真实写出的产物字节，而不是按文件名或版本号推算。
    let mut digests = Vec::new();
    for js in &js_outputs {
        digests.push(digest_of(output_dir, &js.relative_path)?);
    }
    for css in &css_outputs {
        digests.push(digest_of(output_dir, &css.filename)?);
    }
    digests.push(digest_of(output_dir, "boot.js")?);
    if opts.wasm_built {
        for filename in [format!("{}.wasm", opts.wasm_stem), format!("{}.mjs", opts.wasm_stem)] {
            digests.push(digest_of(output_dir, &filename)?);
        }
    }

    let url_mode = if opts.relative_urls { ManifestUrlMode::Relative } else { ManifestUrlMode::Absolute };
    // chunk 条目的 `hash` 同样取自真实落盘字节；与摘要表分开保留以兼容既有条目形态。
    let wasm_content_hash = if opts.wasm_built { hash_wasm_chunk(output_dir, &opts.wasm_stem) } else { None };
    let manifest = generate_manifest_with_urls(
        &opts.module_stem,
        &opts.mode,
        components,
        &js_outputs,
        &opts.wasm_stem,
        opts.wasm_built,
        url_mode,
        wasm_content_hash.as_deref(),
        &digests,
        &css_filenames,
        opts.css_mode == "per-component",
        &opts.porting_cost,
    );
    fs::write(output_dir.join(&manifest.relative_path), &manifest.content)?;

    Ok(IslandPackageReport { js_file_count: js_outputs.len(), css_filenames, routes })
}

/// 从已落盘 wasm 的 `nyar.host_providers` 段读出语言 × 目标适配器提交的宿主导入
/// provider，渲染为 boot 壳的注入片段：`(辅助 JS, 绑定 JS)`。
///
/// 文本操作（`utf8_*` / `utf16_*`）是语言语义，宿主壳不内建实现；与 Node glue
/// 同一合同（`compiler.emitter::backend::wasm_js_glue`）。段缺失时返回空片段
/// （纯通用装载模块照常工作）；段形状不符即结构化失败，绝不静默丢绑定。
fn host_provider_js(output_dir: &Path, wasm_stem: &str) -> AsgardResult<(String, String)> {
    let path = output_dir.join(format!("{wasm_stem}.wasm"));
    let Ok(bytes) = fs::read(&path) else {
        return Ok((String::new(), String::new()));
    };
    let module = data_binary_wasm::WasmBinaryModule::from_bytes(&bytes).map_err(|error| AsgardError::msg(format!("WASM 解析失败：{error}")))?;
    let Some(section) = module.sections.iter().find(|section| section.name.as_deref() == Some("nyar.host_providers")) else {
        return Ok((String::new(), String::new()));
    };
    render_host_provider_spec(&String::from_utf8_lossy(&section.bytes))
}

/// 把 `nyar.host_providers` 段 JSON 渲染为 boot 壳注入片段：`(辅助 JS, 绑定 JS)`。
fn render_host_provider_spec(spec_json: &str) -> AsgardResult<(String, String)> {
    let spec: serde_json::Value = serde_json::from_str(spec_json).map_err(|error| AsgardError::msg(format!("`nyar.host_providers` 段 JSON 解析失败: {error}")))?;
    let providers = spec.get("providers").and_then(serde_json::Value::as_array).ok_or_else(|| AsgardError::msg("`nyar.host_providers` 段缺少 `providers` 数组"))?;

    let mut helpers = String::new();
    let mut bindings: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for provider in providers {
        let field = provider.get("field").and_then(serde_json::Value::as_str).ok_or_else(|| AsgardError::msg("`nyar.host_providers` 条目缺少 `field`"))?;
        let module_name = provider.get("module").and_then(serde_json::Value::as_str).unwrap_or("env");
        let implementation = provider.get("implementation").and_then(serde_json::Value::as_str).ok_or_else(|| AsgardError::msg(format!("provider `{field}` 缺少 `implementation`")))?;
        if let Some(helper) = provider.get("helpers").and_then(serde_json::Value::as_str) {
            let helper = helper.trim();
            if !helper.is_empty() && !helpers.contains(helper) {
                helpers.push_str(helper);
                helpers.push_str("\n\n");
            }
        }
        bindings.entry(module_name.to_string()).or_default().push(format!("    {}: {implementation}", serde_json::to_string(field).map_err(|error| AsgardError::msg(format!("provider 字段名序列化失败: {error}")))?));
    }

    let mut rendered = String::new();
    for (module_name, entries) in bindings {
        let entries = entries.join(",\n");
        if module_name == "env" {
            rendered.push_str(&format!("Object.assign(env, {{\n{entries}\n}});\n"));
        }
        else {
            rendered.push_str(&format!("hostImportModules[{}] = Object.assign(hostImportModules[{}] || {{}}, {{\n{entries}\n}});\n", serde_json::to_string(&module_name).unwrap(), serde_json::to_string(&module_name).unwrap()));
        }
    }
    Ok((helpers, rendered))
}

/// 读取真实产物并按字节计算小写十六进制 SHA-256；缺失产物直接失败闭合。
fn digest_of(output_dir: &Path, relative_path: &str) -> AsgardResult<ManifestDigest> {
    let normalized = relative_path.replace('\\', "/");
    let path = output_dir.join(relative_path);
    let bytes = fs::read(&path).map_err(|e| AsgardError::msg(format!("{e}: 读取产物失败 {}", path.display())))?;
    let digest = Sha256::digest(&bytes);
    let sha256: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(ManifestDigest { path: normalized, sha256 })
}

#[cfg(test)]
mod tests {
    use super::render_host_provider_spec;

    #[test]
    fn host_provider_section_renders_env_bindings_and_helpers_once() {
        let spec = r#"{"providers":[
            {"module":"env","field":"utf8_concat","implementation":"(a, b) => hostUtf8Concat(a, b)","helpers":"function hostUtf8Concat(a, b) { return hostIntern(a + b); }"},
            {"module":"env","field":"utf16_concat","implementation":"(a, b) => hostUtf16Concat(a, b)","helpers":"function hostUtf8Concat(a, b) { return hostIntern(a + b); }"}
        ]}"#;
        let (helpers, bindings) = render_host_provider_spec(spec).expect("段渲染应成功");
        assert_eq!(helpers.matches("function hostUtf8Concat").count(), 1, "共享辅助 JS 只注入一次：{helpers}");
        assert!(bindings.contains("Object.assign(env, {"), "{bindings}");
        assert!(bindings.contains("\"utf8_concat\": (a, b) => hostUtf8Concat(a, b)"), "{bindings}");
        assert!(bindings.contains("\"utf16_concat\": (a, b) => hostUtf16Concat(a, b)"), "utf16 家族不得缺席（boot 硬编码已删）：{bindings}");
    }

    #[test]
    fn host_provider_section_rejects_missing_providers_array() {
        let error = render_host_provider_spec(r#"{"entries":[]}"#).expect_err("缺 `providers` 数组必须失败闭合");
        assert!(error.to_string().contains("nyar.host_providers"), "{error}");
    }
}
