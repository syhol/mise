use crate::aqua::aqua_template;
use crate::backend::aqua;
use crate::backend::aqua::{arch, os};
use crate::config::SETTINGS;
use crate::duration::DAILY;
use crate::git::Git;
use crate::{dirs, file, hashmap, http};
use eyre::Result;
use indexmap::IndexSet;
use itertools::Itertools;
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use url::Url;

pub static AQUA_REGISTRY: Lazy<AquaRegistry> = Lazy::new(|| {
    AquaRegistry::standard().unwrap_or_else(|err| {
        warn!("failed to initialize aqua registry: {err:?}");
        AquaRegistry::default()
    })
});
static AQUA_REGISTRY_PATH: Lazy<PathBuf> = Lazy::new(|| dirs::CACHE.join("aqua-registry"));

#[derive(Default)]
pub struct AquaRegistry {
    path: PathBuf,
    repo_exists: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "snake_case")]
pub enum AquaPackageType {
    #[default]
    GithubRelease,
    Http,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct AquaPackage {
    pub r#type: AquaPackageType,
    pub repo_owner: String,
    pub repo_name: String,
    pub asset: String,
    pub url: String,
    pub description: Option<String>,
    pub format: String,
    pub rosetta2: bool,
    pub windows_arm_emulation: bool,
    pub complete_windows_ext: bool,
    pub supported_envs: Vec<String>,
    pub files: Vec<AquaFile>,
    pub replacements: HashMap<String, String>,
    overrides: Vec<AquaOverride>,
    version_constraint: String,
    version_overrides: Vec<AquaPackage>,
}

#[derive(Debug, Deserialize, Clone)]
struct AquaOverride {
    #[serde(flatten)]
    pkg: AquaPackage,
    goos: Option<String>,
    goarch: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AquaFile {
    pub name: String,
    pub src: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryYaml {
    packages: Vec<AquaPackage>,
}

impl AquaRegistry {
    pub fn standard() -> Result<Self> {
        let path = AQUA_REGISTRY_PATH.clone();
        let repo = Git::new(&path);
        let mut repo_exists = repo.exists();
        if repo_exists {
            fetch_latest_repo(&repo)?;
        } else if let Some(aqua_registry_url) = &SETTINGS.aqua_registry_url {
            info!("cloning aqua registry to {path:?}");
            repo.clone(aqua_registry_url)?;
            repo_exists = true;
        }
        Ok(Self { path, repo_exists })
    }

    pub fn package(&self, id: &str) -> Result<Option<AquaPackage>> {
        let path_id = id.split('/').join(std::path::MAIN_SEPARATOR_STR);
        let path = self.path.join("pkgs").join(&path_id).join("registry.yaml");
        if !self.repo_exists && (!path.exists() || file::modified_duration(&path)? > DAILY) {
            let url: Url =
                format!("https://mise-versions.jdx.dev/aqua-registry/{path_id}/registry.yaml")
                    .parse()?;
            http::HTTP_FETCH.download_file(url, &path, None)?;
        }
        let f = file::open(&path)?;
        let registry: RegistryYaml = serde_yaml::from_reader(f)?;
        Ok(registry.packages.into_iter().next())
    }

    pub fn package_with_version(&self, id: &str, v: &str) -> Result<Option<AquaPackage>> {
        if let Some(pkg) = self.package(id)? {
            Ok(Some(pkg.with_version(v)))
        } else {
            Ok(None)
        }
    }
}

fn fetch_latest_repo(repo: &Git) -> Result<()> {
    if file::modified_duration(&repo.dir)? < DAILY {
        return Ok(());
    }
    info!("updating aqua registry repo");
    repo.update(None)?;
    Ok(())
}

impl AquaPackage {
    fn with_version(mut self, v: &str) -> AquaPackage {
        if let Some(avo) = self.version_override(v).cloned() {
            self = apply_override(self, &avo)
        }
        if let Some(avo) = self.overrides.clone().into_iter().find(|o| {
            if let (Some(goos), Some(goarch)) = (&o.goos, &o.goarch) {
                goos == aqua::os() && goarch == aqua::arch()
            } else if let Some(goos) = &o.goos {
                goos == aqua::os()
            } else if let Some(goarch) = &o.goarch {
                goarch == aqua::arch()
            } else {
                false
            }
        }) {
            self = apply_override(self, &avo.pkg)
        }
        self
    }

    fn version_override(&self, _v: &str) -> Option<&AquaPackage> {
        self.version_overrides
            .iter()
            // TODO: semver
            .find(|vo| vo.version_constraint == "true")
    }

    pub fn format(&self, v: &str) -> &str {
        if self.format.is_empty() {
            let asset = self.asset(v);
            if asset.ends_with(".tar.gz") {
                "tar.gz"
            } else if asset.ends_with(".tar.xz") {
                "tar.xz"
            } else if asset.ends_with(".gz") {
                "gz"
            } else if asset.ends_with(".xz") {
                "xz"
            } else if asset.ends_with(".zip") {
                "zip"
            } else {
                "raw"
            }
        } else {
            &self.format
        }
    }

    pub fn asset(&self, v: &str) -> String {
        self.parse_aqua_str(&self.asset, v, &Default::default())
    }

    pub fn asset_strs(&self, v: &str) -> IndexSet<String> {
        let mut strs = IndexSet::from([self.asset(v)]);
        if cfg!(macos) {
            let mut ctx = HashMap::default();
            ctx.insert("ARCH".to_string(), "arm64".to_string());
            strs.insert(self.parse_aqua_str(&self.asset, v, &ctx));
        }
        strs
    }

    pub fn url(&self, v: &str) -> String {
        self.parse_aqua_str(&self.url, v, &Default::default())
    }

    fn parse_aqua_str(&self, s: &str, v: &str, overrides: &HashMap<String, String>) -> String {
        let os = os();
        let mut arch = arch();
        if os == "darwin" && arch == "arm64" && self.rosetta2 {
            arch = "amd64";
        }
        if os == "windows" && arch == "arm64" && self.windows_arm_emulation {
            arch = "amd64";
        }
        let replace = |s: &str| {
            self.replacements
                .get(s)
                .map(|s| s.to_string())
                .unwrap_or_else(|| s.to_string())
        };
        let mut ctx = hashmap! {
            "Version".to_string() => replace(v),
            "OS".to_string() => replace(os),
            "Arch".to_string() => replace(arch),
            "Format".to_string() => replace(&self.format),
        };
        ctx.extend(overrides.clone());
        aqua_template::render(s, &ctx)
    }
}

impl AquaFile {
    pub fn src(&self, pkg: &AquaPackage, v: &str) -> Option<String> {
        self.src
            .as_ref()
            .map(|src| pkg.parse_aqua_str(src, v, &Default::default()))
    }
}

fn apply_override(mut orig: AquaPackage, avo: &AquaPackage) -> AquaPackage {
    if !avo.repo_owner.is_empty() {
        orig.repo_owner = avo.repo_owner.clone();
    }
    if !avo.repo_name.is_empty() {
        orig.repo_name = avo.repo_name.clone();
    }
    if !avo.asset.is_empty() {
        orig.asset = avo.asset.clone();
    }
    if !avo.url.is_empty() {
        orig.url = avo.url.clone();
    }
    if !avo.format.is_empty() {
        orig.format = avo.format.clone();
    }
    if avo.rosetta2 {
        orig.rosetta2 = true;
    }
    if avo.windows_arm_emulation {
        orig.windows_arm_emulation = true;
    }
    if avo.complete_windows_ext {
        orig.complete_windows_ext = true;
    }
    if !avo.supported_envs.is_empty() {
        orig.supported_envs = avo.supported_envs.clone();
    }
    if !avo.files.is_empty() {
        orig.files = avo.files.clone();
    }
    orig.replacements.extend(avo.replacements.clone());
    if !avo.overrides.is_empty() {
        orig.overrides = avo.overrides.clone();
    }

    orig
}
