use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

const WEBKIT_REPO: &str = "https://github.com/WebKit/WebKit.git";
const UI_SUBDIR: &str = "Source/WebInspectorUI/UserInterface";
const ADDITIONS_JS: &str = include_str!("../assets/additions.js");
const ADDITIONS_CSS: &str = include_str!("../assets/additions.css");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
    pub path: PathBuf,
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug)]
pub struct Frontend {
    root: PathBuf,
    versions: Vec<ProtocolVersion>,
    fallback: Option<PathBuf>,
}

impl Frontend {
    pub fn open(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("frontend directory {} does not exist", root.display()))?;
        if !root.join("Main.html").is_file() {
            bail!(
                "{} does not look like a WebKit WebInspectorUI directory (Main.html is missing)",
                root.display()
            );
        }

        let legacy_root = root.join("Protocol/Legacy/iOS");
        let mut versions = Vec::new();
        if legacy_root.is_dir() {
            for entry in std::fs::read_dir(&legacy_root)
                .with_context(|| format!("failed to read {}", legacy_root.display()))?
            {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Some((major, minor)) = parse_version(&name) else {
                    continue;
                };
                let path = entry.path().join("InspectorBackendCommands.js");
                if path.is_file() {
                    versions.push(ProtocolVersion { major, minor, path });
                }
            }
        }
        versions.sort();

        let fallback = root.join("Protocol/InspectorBackendCommands.js");
        let fallback = fallback.is_file().then_some(fallback);

        if versions.is_empty() && fallback.is_none() {
            bail!(
                "{} has no Protocol/Legacy/iOS protocol files and no Protocol/InspectorBackendCommands.js",
                root.display()
            );
        }

        if let Err(error) = ensure_native_function_parameters(&root) {
            warn!("could not generate Models/NativeFunctionParameters.js: {error:#}");
        }

        Ok(Self {
            root,
            versions,
            fallback,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn protocol_versions(&self) -> &[ProtocolVersion] {
        &self.versions
    }

    /// Pick the newest protocol version that is not newer than the device OS.
    /// Falls back to the newest available version when there is no exact match.
    pub fn protocol_for(&self, os_version: (u32, u32, u32)) -> Option<&ProtocolVersion> {
        if self.versions.is_empty() {
            return None;
        }
        if os_version == (0, 0, 0) {
            return self.versions.last();
        }

        let wanted = (os_version.0, os_version.1);
        let mut chosen = None;
        for version in &self.versions {
            if (version.major, version.minor) <= wanted {
                chosen = Some(version);
            } else {
                break;
            }
        }
        chosen.or_else(|| self.versions.first())
    }

    pub fn protocol_path_for(&self, os_version: (u32, u32, u32)) -> Option<&Path> {
        self.protocol_for(os_version)
            .map(|v| v.path.as_path())
            .or(self.fallback.as_deref())
    }
}

fn parse_version(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

pub fn cache_root() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("cannot determine a cache directory")?;
    Ok(base.join("safari-debug"))
}

pub fn default_ui_dir() -> Result<PathBuf> {
    Ok(cache_root()?.join("WebKit").join(UI_SUBDIR))
}

pub fn looks_like_frontend(dir: &Path) -> bool {
    dir.join("Main.html").is_file()
}

pub async fn install(tag: &str, force: bool) -> Result<PathBuf> {
    let webkit_dir = cache_root()?.join("WebKit");
    let ui_dir = webkit_dir.join(UI_SUBDIR);

    if looks_like_frontend(&ui_dir) && !force {
        ensure_additions(&ui_dir)?;
        ensure_native_function_parameters(&ui_dir)?;
        info!("WebKit frontend already installed at {}", ui_dir.display());
        return Ok(ui_dir);
    }

    if webkit_dir.exists() {
        if force {
            info!("Removing old WebKit checkout at {}", webkit_dir.display());
            tokio::fs::remove_dir_all(&webkit_dir)
                .await
                .with_context(|| format!("failed to remove {}", webkit_dir.display()))?;
        } else if !looks_like_frontend(&ui_dir) {
            info!(
                "Removing incomplete WebKit checkout at {}",
                webkit_dir.display()
            );
            tokio::fs::remove_dir_all(&webkit_dir)
                .await
                .with_context(|| format!("failed to remove {}", webkit_dir.display()))?;
        }
    }

    if let Some(parent) = webkit_dir.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    info!("Cloning WebKit (sparse, filter=blob:none) at ref {tag}");
    info!("This downloads roughly 100 MB and only takes a minute on a decent connection.");

    let status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            "--branch",
            tag,
            WEBKIT_REPO,
        ])
        .arg(&webkit_dir)
        .status()
        .await
        .context("failed to run git; install git and try again")?;
    if !status.success() {
        let _ = tokio::fs::remove_dir_all(&webkit_dir).await;
        bail!(
            "git clone failed for ref {tag}. Use a branch or tag name such as main, \
             releases/Apple/Safari-18.3-iOS-18.3, or another release tag from \
             https://github.com/WebKit/WebKit/tags"
        );
    }

    info!("Checking out Source/WebInspectorUI/UserInterface");
    let status = Command::new("git")
        .current_dir(&webkit_dir)
        .args([
            "sparse-checkout",
            "set",
            "Source/WebInspectorUI/UserInterface",
        ])
        .status()
        .await
        .context("failed to run git sparse-checkout")?;
    if !status.success() {
        bail!("git sparse-checkout failed");
    }

    if !looks_like_frontend(&ui_dir) {
        bail!(
            "WebKit checkout finished but {} is missing. The selected ref may not contain WebInspectorUI.",
            ui_dir.display()
        );
    }

    ensure_additions(&ui_dir)?;
    ensure_native_function_parameters(&ui_dir)?;

    // Make sure a fallback protocol exists for clients that do not hit our
    // per-device protocol route.
    let latest = std::fs::read_dir(ui_dir.join("Protocol/Legacy/iOS"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let (major, minor) = parse_version(&name)?;
            let js = e.path().join("InspectorBackendCommands.js");
            js.is_file().then_some(((major, minor), js))
        })
        .max_by_key(|(v, _)| *v);
    if let Some((_, js)) = latest {
        let dest = ui_dir.join("Protocol/InspectorBackendCommands.js");
        if !dest.exists() {
            std::fs::copy(&js, &dest)
                .with_context(|| format!("failed to copy {}", dest.display()))?;
        }
    }

    info!("Frontend installed at {}", ui_dir.display());
    Ok(ui_dir)
}

fn ensure_additions(ui_dir: &Path) -> Result<()> {
    let additions = ui_dir.join("WebKitAdditions/WebInspectorUI");
    std::fs::create_dir_all(&additions)
        .with_context(|| format!("failed to create {}", additions.display()))?;

    let js = additions.join("WebInspectorUIAdditions.js");
    std::fs::write(&js, ADDITIONS_JS)
        .with_context(|| format!("failed to write {}", js.display()))?;

    let css = additions.join("WebInspectorUIAdditions.css");
    std::fs::write(&css, ADDITIONS_CSS)
        .with_context(|| format!("failed to write {}", css.display()))?;

    let protocol_dir = ui_dir.join("Protocol/Legacy/iOS");
    if !protocol_dir.is_dir() {
        warn!(
            "{} has no Legacy/iOS protocol directory; the inspector may not work",
            ui_dir.display()
        );
    }
    Ok(())
}

/// WebKit's build generates `Models/NativeFunctionParameters.js` from WebCore
/// IDL plus the checked-in overrides. A sparse WebInspectorUI checkout does not
/// contain that generated file, so synthesize it from the overrides. The UI
/// uses it for console autocomplete/parameter hints; without it the browser
/// logs a 404 for the `<script>` tag.
fn ensure_native_function_parameters(ui_dir: &Path) -> Result<()> {
    let output = ui_dir.join("Models/NativeFunctionParameters.js");
    if output.is_file() {
        return Ok(());
    }

    let overrides_path = ui_dir.join("Models/NativeFunctionParameters-overrides.json");
    let text = match std::fs::read_to_string(&overrides_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", overrides_path.display()));
        }
    };

    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", overrides_path.display()))?;
    let object = value
        .as_object()
        .context("native function parameter overrides are not an object")?;

    let mut constructors = BTreeMap::new();
    let mut prototypes = BTreeMap::new();
    for (class_name, entry) in object {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        if let Some(methods) = entry.get("constructor").and_then(|v| v.as_object()) {
            constructors.insert(class_name.clone(), methods_from(methods));
        }
        if let Some(methods) = entry.get("prototype").and_then(|v| v.as_object()) {
            prototypes.insert(class_name.clone(), methods_from(methods));
        }
    }

    let mut contents = String::new();
    contents.push_str(
        "// Generated by safari-debug from Models/NativeFunctionParameters-overrides.json.\n",
    );
    contents.push_str(
        "// Upstream generator: Source/WebInspectorUI/Scripts/update-NativeFunctionParameters.py\n\n",
    );
    contents.push_str(&generate_parameter_object(
        "WI.NativeConstructorFunctionParameters",
        &constructors,
    ));
    contents.push('\n');
    contents.push_str(&generate_parameter_object(
        "WI.NativePrototypeFunctionParameters",
        &prototypes,
    ));

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&output, contents)
        .with_context(|| format!("failed to write {}", output.display()))?;
    info!("Generated {}", output.display());
    Ok(())
}

fn methods_from(object: &serde_json::Map<String, serde_json::Value>) -> BTreeMap<String, String> {
    object
        .iter()
        .filter_map(|(name, value)| {
            value
                .as_str()
                .map(|params| (name.clone(), params.to_string()))
        })
        .collect()
}

fn generate_parameter_object(
    target: &str,
    data: &BTreeMap<String, BTreeMap<String, String>>,
) -> String {
    let mut output = String::new();
    output.push_str(target);
    output.push_str(" = {\n");
    for (class_name, methods) in data {
        if methods.is_empty() {
            continue;
        }
        output.push_str("    ");
        output.push_str(&js_key(class_name));
        output.push_str(": {\n");
        for (method, params) in methods {
            output.push_str("        ");
            output.push_str(&js_key(method));
            output.push_str(": ");
            output.push_str(
                &serde_json::to_string(params).expect("string serialization cannot fail"),
            );
            output.push_str(",\n");
        }
        output.push_str("        __proto__: null,\n");
        output.push_str("    },\n");
    }
    output.push_str("};\n");
    output
}

fn js_key(name: &str) -> String {
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .map(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '$')
        .unwrap_or(false);
    let valid_rest = chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$');
    if valid_start && valid_rest {
        name.to_string()
    } else {
        serde_json::to_string(name).expect("string serialization cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_native_function_parameters() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "safari-debug-native-params-{}-{suffix}",
            std::process::id()
        ));
        let models = dir.join("Models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(
            models.join("NativeFunctionParameters-overrides.json"),
            r#"{"Array":{"constructor":{"from":"arrayLike"},"prototype":{"map":"callback"}}}"#,
        )
        .unwrap();

        ensure_native_function_parameters(&dir).unwrap();
        let text = std::fs::read_to_string(models.join("NativeFunctionParameters.js")).unwrap();
        assert!(text.contains("WI.NativeConstructorFunctionParameters"));
        assert!(text.contains("from: \"arrayLike\""));
        assert!(text.contains("WI.NativePrototypeFunctionParameters"));
        assert!(text.contains("map: \"callback\""));
        assert!(text.contains("__proto__: null"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
