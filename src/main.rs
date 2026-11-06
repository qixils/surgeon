use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::Client;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::digest::DynDigest;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Serialize, Deserialize, ValueEnum, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Modloader {
    Paper,
    Fabric,
    #[value(name = "neoforge")]
    NeoForge,
}

#[derive(Parser)]
#[command(version, about, long_about = None, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initializes a project
    Init(InitArgs),
    /// Downloads one or more Modrinth projects
    /// Projects are downloaded to an appropriate local folder based on their metadata, such as mod vs plugin
    Download(DownloadArgs),
    /// Updates all downloaded projects
    Update,
}

#[derive(Args)]
struct InitArgs {
    modloader: Modloader,
    /// Minecraft version
    mc_version: String,
    /// Default minimum release channel for downloads
    #[arg(short, long, value_enum, default_value_t = Channel::Release)]
    channel: Channel,
}

#[derive(Args)]
struct DownloadArgs {
    /// Modrinth project ID(s) and/or slug(s)
    #[arg(required = true)]
    id: Vec<String>,
    /// Additional Minecraft version(s) to accept
    #[arg(short = 'g', long = "game-version", value_name = "MC_VERSION")]
    game_versions: Vec<String>,
    /// Custom minimum release channel for this download and its dependencies
    #[arg(short, long, value_enum)]
    channel: Option<Channel>,
}

#[derive(
    Serialize, Deserialize, ValueEnum, Clone, Copy, PartialEq, PartialOrd, Debug, Default,
)]
#[serde(rename_all = "lowercase")]
enum Channel {
    Alpha,
    Beta,
    #[default]
    Release,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum DependencyType {
    Required,
    Optional,
    Incompatible,
    Embedded,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ProjectVersionDependency {
    version_id: Option<String>,
    project_id: Option<String>,
    dependency_type: DependencyType,
}

#[derive(Deserialize, Default)]
struct ProjectVersionFileHashes {
    sha1: Option<String>,
    sha512: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ProjectVersionFile {
    primary: bool,
    url: String,
    filename: String,
    /// required-resource-pack, optional-resource-pack, sources-jar, dev-jar, javadoc-jar, unknown, signature
    file_type: Option<String>,
    #[serde(default)]
    hashes: ProjectVersionFileHashes,
}

impl ProjectVersionFile {
    /// Fetch the strongest provided hash
    fn hash(&self) -> Option<FileHash> {
        self.hashes.sha512
            .as_ref()
            .map(|h| FileHash::Sha512(h.clone()))
            .or_else(|| self.hashes.sha1.as_ref().map(|h| FileHash::Sha1(h.clone())))
    }
}

/// Hex-encoded file digest
#[derive(Clone, Debug)]
enum FileHash {
    Sha1(String),
    Sha256(String),
    Sha512(String),
}

impl FileHash {
    /// Parses GitHub-style `algorithm:hex` digests
    fn from_prefixed(digest: &str) -> Option<FileHash> {
        let (algorithm, hex) = digest.split_once(':')?;
        let hex = hex.to_owned();
        match algorithm {
            "sha1" => Some(FileHash::Sha1(hex)),
            "sha256" => Some(FileHash::Sha256(hex)),
            "sha512" => Some(FileHash::Sha512(hex)),
            _ => None,
        }
    }

    fn expected(&self) -> &str {
        match self {
            FileHash::Sha1(h) | FileHash::Sha256(h) | FileHash::Sha512(h) => h,
        }
    }

    fn hasher(&self) -> Box<dyn DynDigest + Send> {
        match self {
            FileHash::Sha1(_) => Box::new(sha1::Sha1::default()),
            FileHash::Sha256(_) => Box::new(sha2::Sha256::default()),
            FileHash::Sha512(_) => Box::new(sha2::Sha512::default()),
        }
    }

    fn matches(&self, actual: &str) -> bool {
        self.expected().eq_ignore_ascii_case(actual)
    }
}

/// Hashes an existing file on disk with the algorithm of `hash`, returning the hex digest
async fn hash_file(path: &str, hash: &FileHash) -> tokio::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = hash.hasher();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Successful state of a file download
#[derive(PartialEq)]
enum FileStatus {
    Downloaded,
    AlreadyPresent,
}

fn report_file(path: &str, status: &FileStatus) {
    match status {
        FileStatus::Downloaded => println!("Downloaded {path}"),
        FileStatus::AlreadyPresent => println!("{path} is up to date"),
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ProjectVersion {
    id: String,
    project_id: String,
    version_type: Option<Channel>,
    loaders: Option<Vec<String>>,
    dependencies: Option<Vec<ProjectVersionDependency>>,
    game_versions: Option<Vec<String>>,
    files: Vec<ProjectVersionFile>,
}

impl ProjectVersion {
    /// Fetches the file flagged as primary
    fn primary_file(&self) -> Option<&ProjectVersionFile> {
        self.files.iter().find(|f| f.primary)
            .or_else(|| self.files.first()) // fallback is documented as being impossible but just in case
    }

    /// Whether this project version supports the provided Minecraft version
    fn supports_game_version(&self, game_version: &str) -> bool {
        self.game_versions
            .as_ref()
            .is_some_and(|gvs| gvs.iter().any(|v| v == game_version))
    }
}

#[derive(Deserialize)]
struct FabricInstaller {
    version: String,
    stable: bool,
    // + url + maven
}

#[derive(Deserialize)]
struct FabricLoader {
    version: String,
    stable: bool,
    // + separator + build + maven
}

#[derive(Deserialize)]
struct GithubRelease {
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    /// e.g. `sha256:abc...`
    digest: Option<String>,
}

#[derive(Deserialize)]
struct MavenVersions {
    versions: Vec<String>,
}

#[derive(Deserialize)]
struct PaperBuild {
    id: u64,
    // todo enum
    /// ALPHA, BETA, STABLE
    channel: String,
    downloads: HashMap<String, PaperDownload>,
}

#[derive(Deserialize)]
struct PaperDownload {
    checksums: PaperChecksums,
    url: String,
}

#[derive(Deserialize)]
struct PaperChecksums {
    sha256: String,
}

#[derive(Debug, Error)]
enum DownloadError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("no {channel:?} (or more stable) version for {game_versions:?}")]
    NoMatchingVersion {
        channel: Channel,
        game_versions: Vec<String>,
    },
    #[error("version {0} has no files")]
    NoFiles(String),
    #[error("unable to obtain {0} data from {1} web API")]
    NoAPI(String, String),
    #[error("no {loader} version found for Minecraft {mc_version}")]
    NoLoaderVersion { loader: String, mc_version: String },
    #[error("hash mismatch for {file}: expected {expected}, got {actual}")]
    HashMismatch {
        file: String,
        expected: String,
        actual: String,
    },
}

/// Describes how/why the requested project is on the latest version
enum LatestVersion {
    /// The project was already handled earlier this run
    AlreadyResolved,
    /// The latest version is already installed from a previous session
    UpToDate(ProjectVersion),
    /// The latest version was freshly downloaded
    Downloaded(ProjectVersion),
}

#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct DownloadedProject {
    /// Last installed Modrinth version ID
    version_id: String,
    /// Modrinth project ID
    project_id: String,
    /// Last installed filename
    filename: String,
    /// Whether this was installed as a depndency of another mod
    as_dependency: bool,
    /// Required dependencies' project IDs
    dependencies: Vec<String>,
    /// The last Minecraft version this was installed for.
    mc_version: String,
    /// The last explicitly specified minimum release channel.
    channel: Option<Channel>,
}

#[derive(Serialize, Deserialize)]
struct DownloadClientMetadata {
    modloader: Modloader,
    mc_version: String,
    /// Default minimum release channel, used when a project has none saved
    #[serde(default)]
    channel: Channel,
    downloads: Vec<DownloadedProject>,
}

struct DownloadClient {
    /// HTTP client
    client: Client,
    /// Project metadata
    metadata: DownloadClientMetadata,
    /// Folder to save downloads into
    dest: String,
}

fn dest_for(modloader: Modloader) -> String {
    // TODO: improve this!
    if modloader == Modloader::Paper {
        "plugins".to_owned()
    } else {
        "mods".to_owned()
    }
}

impl DownloadClient {
    async fn new() -> DownloadClient {
        let metadata = tokio::fs::read_to_string("surgeon.json").await
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_else(|| DownloadClientMetadata {
                modloader: Modloader::Fabric,
                mc_version: "invalid".to_owned(),
                channel: Channel::Release,
                downloads: Vec::new(),
            });
        let dest = dest_for(metadata.modloader);
        DownloadClient {
            client: Client::builder()
                // TODO: version number
                .user_agent("qixils/surgeon/1.0.0 (lexi@qixils.dev)")
                .build()
                .expect("failed to build HTTP client"),
            metadata,
            dest,
        }
    }

    fn set_metadata(&mut self, metadata: DownloadClientMetadata) {
        self.dest = dest_for(metadata.modloader);
        self.metadata = metadata;
    }

    async fn save_metadata(&self) -> std::io::Result<()> {
        let contents = serde_json::to_string_pretty(&self.metadata)?;
        tokio::fs::write("surgeon.json", contents).await?;
        Ok(())
    }

    /// Destination filepath for a Modrinth project
    fn mod_path(&self, filename: &str) -> String {
        return format!("{}/{}", &self.dest, filename);
    }

    /// Streams the file at `url` to `path`.
    /// `hash` optionally allows skipping downloading an existing file and validating a resulting file.
    async fn download_file(
        &self,
        url: &str,
        path: &str,
        hash: Option<&FileHash>,
    ) -> Result<FileStatus, DownloadError> {
        if let Some(hash) = hash
            && let Ok(actual) = hash_file(path, hash).await
            && hash.matches(&actual)
        {
            return Ok(FileStatus::AlreadyPresent);
        }

        let mut resp = self.client.get(url).send().await?.error_for_status()?;
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let part_path = format!("{path}.part");

        let result: Result<(), DownloadError> = async {
            let mut file = tokio::fs::File::create(&part_path).await?;
            let mut hasher = hash.map(FileHash::hasher);
            while let Some(chunk) = resp.chunk().await? {
                if let Some(hasher) = &mut hasher {
                    hasher.update(&chunk);
                }
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            if let (Some(hash), Some(hasher)) = (hash, hasher) {
                let actual = hex::encode(hasher.finalize());
                if !hash.matches(&actual) {
                    return Err(DownloadError::HashMismatch {
                        file: path.to_owned(),
                        expected: hash.expected().to_owned(),
                        actual,
                    });
                }
            }
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                tokio::fs::rename(&part_path, path).await?;
                Ok(FileStatus::Downloaded)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&part_path).await;
                Err(e)
            }
        }
    }

    /// Downloads the latest Fabric
    async fn download_fabric(&self) -> Result<(), DownloadError> {
        let installers: Vec<FabricInstaller> = self.client.get("https://meta.fabricmc.net/v2/versions/installer").send().await?.error_for_status()?.json().await?;
        let installer = &installers.iter().find(|v| v.stable).or_else(|| installers.first()).ok_or_else(|| DownloadError::NoAPI("installer".to_owned(), "Fabric".to_owned()))?.version;
        let loaders: Vec<FabricLoader> = self.client.get("https://meta.fabricmc.net/v2/versions/loader").send().await?.error_for_status()?.json().await?;
        let loader = &loaders.iter().find(|v| v.stable).or_else(|| loaders.first()).ok_or_else(|| DownloadError::NoAPI("loader".to_owned(), "Fabric".to_owned()))?.version;
        let mc_version = &self.metadata.mc_version;
        let path = "fabric-launcher.jar";
        let status = self
            .download_file(
                &format!("https://meta.fabricmc.net/v2/versions/loader/{mc_version}/{loader}/{installer}/server/jar"),
                path,
                None,
            )
            .await?;
        report_file(path, &status);
        // TODO: log version numbers to cli
        Ok(())
    }

    /// Downloads the latest NeoForge server starter and installer
    async fn download_neoforge(&self) -> Result<(), DownloadError> {
        // Server starter (launcher)
        let release: GithubRelease = self
            .client
            .get("https://api.github.com/repos/neoforged/ServerStarterJar/releases/latest")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == "server.jar")
            .ok_or_else(|| DownloadError::NoAPI("server.jar".to_owned(), "GitHub".to_owned()))?;
        let hash = asset.digest.as_deref().and_then(FileHash::from_prefixed);
        let path = "neoforge-launcher.jar";
        let status = self
            .download_file(&asset.browser_download_url, path, hash.as_ref())
            .await?;
        report_file(path, &status);

        // Installer
        let mc_version = &self.metadata.mc_version;
        let no_version = || DownloadError::NoLoaderVersion {
            loader: "NeoForge".to_owned(),
            mc_version: mc_version.clone(),
        };
        let prefix = neoforge_prefix(mc_version).ok_or_else(no_version)?;
        let maven: MavenVersions = self
            .client
            .get("https://maven.neoforged.net/api/maven/versions/releases/net%2Fneoforged%2Fneoforge")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let version = maven
            .versions
            .iter()
            .filter(|v| v.starts_with(&prefix))
            .filter_map(|v| Some((neoforge_version_key(v)?, v)))
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, v)| v)
            .ok_or_else(no_version)?;
        let url = format!(
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar"
        );
        let sha256 = self
            .client
            .get(format!("{url}.sha256"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let hash = FileHash::Sha256(sha256.trim().to_owned());
        let path = "neoforge-installer.jar";
        let status = self.download_file(&url, path, Some(&hash)).await?;
        report_file(path, &status);
        // TODO: log version numbers to cli
        Ok(())
    }

    /// Downloads the latest Paper
    async fn download_paper(&self) -> Result<(), DownloadError> {
        let mc_version = &self.metadata.mc_version;
        let no_version = || DownloadError::NoLoaderVersion {
            loader: "Paper".to_owned(),
            mc_version: mc_version.clone(),
        };
        let resp = self
            .client
            .get(format!(
                "https://fill.papermc.io/v3/projects/paper/versions/{mc_version}/builds"
            ))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(no_version());
        }
        let builds: Vec<PaperBuild> = resp.error_for_status()?.json().await?;
        let channel_rank = |channel: &str| match channel {
            "STABLE" => 2,
            "BETA" => 1,
            _ => 0,
        };
        let (build, download) = builds
            .iter()
            .filter_map(|b| Some((b, b.downloads.get("server:default")?)))
            .max_by_key(|(b, _)| (channel_rank(&b.channel), b.id))
            .ok_or_else(no_version)?;
        let hash = FileHash::Sha256(download.checksums.sha256.clone());
        let path = "paper-launcher.jar";
        let status = self.download_file(&download.url, path, Some(&hash)).await?;
        report_file(path, &status);
        println!("Paper {mc_version} build {} ({})", build.id, build.channel);
        Ok(())
    }

    /// Gathers all sources of requested mod versions and sorts them by likely best match
    fn sort_game_versions(
        &self,
        extra_game_versions: &[String],
        existing_install: Option<&DownloadedProject>,
    ) -> Vec<String> {
        let mut game_versions: Vec<String> = Vec::new();
        for v in std::iter::once(&self.metadata.mc_version)
            .chain(extra_game_versions)
            .chain(existing_install.map(|e| &e.mc_version))
        {
            if !game_versions.contains(v) {
                game_versions.push(v.clone());
            }
        }
        game_versions
    }

    /// Resolves and downloads the latest suitable version of a project,
    /// if not already downloaded.
    async fn download_latest(
        &self,
        project_id: &str,
        extra_game_versions: &[String],
        min_channel: Option<Channel>,
        resolved: &[ProjectVersion],
    ) -> Result<LatestVersion, DownloadError> {
        if resolved.iter().any(|v| v.project_id == project_id) {
            return Ok(LatestVersion::AlreadyResolved);
        }
        
        let existing_install = self
            .metadata
            .downloads
            .iter()
            .find(|i| i.project_id == project_id);
        let min_channel = min_channel
            .or(existing_install.and_then(|x| x.channel))
            .unwrap_or(self.metadata.channel);
        let game_versions = self.sort_game_versions(extra_game_versions, existing_install);

        let url = Url::parse_with_params(
            &format!("https://api.modrinth.com/v2/project/{project_id}/version"),
            &[
                ("loaders", serde_json::to_string(&[self.metadata.modloader]).unwrap()),
                ("game_versions", serde_json::to_string(&game_versions).unwrap()),
                ("include_changelog", "false".to_owned()),
            ],
        )
            .map_err(|e| DownloadError::InvalidUrl(e.to_string()))?;
        
        let res = self.client.get(url).send().await?.error_for_status()?;
        let data: Vec<ProjectVersion> = res.json().await?;

        let on_channel = |v: &&ProjectVersion| v.version_type.unwrap_or(Channel::Alpha) >= min_channel;
        // Find the first game_version with a published project version (on a compatible channel)
        let index = game_versions
            .iter()
            .find_map(|gv| {
                data.iter()
                    .position(|v| on_channel(&v) && v.supports_game_version(gv))
            })
            .or_else(|| data.iter().position(|v| on_channel(&v)))
            .ok_or_else(|| DownloadError::NoMatchingVersion {
                channel: min_channel,
                game_versions: game_versions.clone(),
            })?;
        let version = data.into_iter().nth(index).expect("index is in bounds");

        // Check if already downloaded
        if resolved.iter().any(|v| v.project_id == version.project_id) {
            return Ok(LatestVersion::AlreadyResolved);
        }

        // Check if already downloaded previously
        let installed = self
            .metadata
            .downloads
            .iter()
            .any(|i| i.project_id == version.project_id && i.version_id == version.id);
        if installed {
            return Ok(LatestVersion::UpToDate(version));
        }

        let file = version
            .primary_file()
            .ok_or_else(|| DownloadError::NoFiles(version.id.clone()))?;
        let status = self
            .download_file(&file.url, &self.mod_path(&file.filename), file.hash().as_ref())
            .await?;
        report_file(&file.filename, &status);

        // Remove the jar from the previous install if orphaned
        if let Some(old) =
            self
                .metadata
                .downloads
                .iter()
                .find(|i| i.project_id == version.project_id)
            && old.filename != file.filename
        {
            match tokio::fs::remove_file(self.mod_path(&old.filename)).await {
                Ok(()) => println!("Removed {}", old.filename),
                Err(e) if e.kind() == tokio::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("Failed to remove {}: {e}", old.filename),
            }
        }

        Ok(LatestVersion::Downloaded(version))
    }

    /// Adds or updates the `downloads` entry for an installed project.
    fn record_install(
        &mut self,
        version: &ProjectVersion,
        dependencies: Vec<String>,
        extra_game_versions: &[String],
        min_channel: Option<Channel>,
        as_dependency: bool,
    ) {
        let index = self
            .metadata
            .downloads
            .iter()
            .position(|d| d.project_id == version.project_id);
        let existing = index.map(|i| &self.metadata.downloads[i]);

        // Fetch the MC version that was matched to prompt this download
        let mc_version = self
            .sort_game_versions(extra_game_versions, existing)
            .into_iter()
            .find(|v| version.supports_game_version(v))
            .unwrap_or_else(|| self.metadata.mc_version.clone());

        let entry = DownloadedProject {
            version_id: version.id.clone(),
            project_id: version.project_id.clone(),
            filename: version
                .primary_file()
                .map(|f| f.filename.clone())
                .or_else(|| existing.map(|e| e.filename.clone()))
                .unwrap_or_default(),
            // prefer `false` if ever used
            as_dependency: existing.map_or(as_dependency, |e| e.as_dependency && as_dependency),
            dependencies,
            mc_version,
            // only saves when explicitly defined
            channel: min_channel.or(existing.and_then(|e| e.channel)),
        };

        match index {
            Some(i) => self.metadata.downloads[i] = entry,
            None => self.metadata.downloads.push(entry),
        }
    }

    /// Downloads a project and its required dependencies.
    /// Records the downloads in local metadata.
    fn download_with_deps<'a>(
        &'a mut self,
        project_id: &'a str,
        extra_game_versions: &'a [String],
        min_channel: Option<Channel>,
        as_dependency: bool,
        resolved: Option<Vec<ProjectVersion>>,
    ) -> Pin<Box<dyn Future<Output = Vec<ProjectVersion>> + 'a>> {
        Box::pin(async move {
            let mut resolved = resolved.unwrap_or_default();
            let item = match self
                .download_latest(project_id, extra_game_versions, min_channel, &resolved)
                .await
            {
                Err(e) => {
                    eprintln!("Failed to download {project_id}: {e}");
                    return resolved;
                }
                Ok(LatestVersion::AlreadyResolved) => return resolved,
                Ok(LatestVersion::UpToDate(v) | LatestVersion::Downloaded(v)) => v,
            };

            let mut dep_ids = Vec::new();
            for dep in item
                .dependencies
                .iter()
                .flatten()
                .filter(|dep| dep.dependency_type == DependencyType::Required)
            {
                if let Some(project_id) = &dep.project_id {
                    dep_ids.push(project_id.clone());
                } else if let Some(version_id) = &dep.version_id {
                    let result: reqwest::Result<ProjectVersion> = async {
                        let res = self
                            .client
                            .get(&format!("https://api.modrinth.com/v2/version/{version_id}"))
                            .send()
                            .await?
                            .error_for_status()?;
                        res.json().await
                    }.await;
                    match result {
                        Ok(data) => dep_ids.push(data.project_id),
                        Err(e) => eprintln!("Failed to resolve dependency version {version_id}: {e}"),
                    }
                }
            }

            self.record_install(
                &item,
                dep_ids.clone(),
                extra_game_versions,
                min_channel,
                as_dependency,
            );
            resolved.push(item);
            
            for dep_id in &dep_ids {
                resolved = self
                    .download_with_deps(
                        dep_id,
                        extra_game_versions,
                        min_channel,
                        true,
                        Some(resolved),
                    )
                    .await;
            }

            resolved
        })
    }
}

/// Maps a Minecraft version to corresponding NeoForge version prefix.
///
/// <2026: `1.X` / `1.X.Y` → `X.0.` / `X.Y.` (e.g. 1.21.1 → `21.1.`)
/// ≥2026: `X.Y` / `X.Y.Z` → `X.Y.0.` / `X.Y.Z.` (e.g. 26.1 → `26.1.0.`)
///
/// Returns `None` for unknown versions.
// TODO: snapshot support
fn neoforge_prefix(mc_version: &str) -> Option<String> {
    let parts: Vec<&str> = mc_version.split('.').collect();
    if parts.iter().any(|p| p.parse::<u64>().is_err()) {
        return None;
    }
    match parts.as_slice() {
        ["1", minor] => Some(format!("{minor}.0.")),
        ["1", minor, patch] => Some(format!("{minor}.{patch}.")),
        [major, minor] => Some(format!("{major}.{minor}.0.")),
        [major, minor, patch] => Some(format!("{major}.{minor}.{patch}.")),
        _ => None,
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum PreReleaseIdent {
    Num(u64),
    Str(String),
}

/// Sort key for NeoForge versions.
fn neoforge_version_key(version: &str) -> Option<(u8, Vec<u64>, Vec<PreReleaseIdent>)> {
    let version = version.split_once('+').map_or(version, |(v, _)| v);
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let numbers = core
        .split('.')
        .map(|n| n.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    let stability = match pre {
        None => 2,
        Some(p) if p.starts_with("beta") => 1,
        Some(_) => 0,
    };
    let pre_idents = pre
        .into_iter()
        .flat_map(|p| p.split('.'))
        .map(|ident| match ident.parse() {
            Ok(n) => PreReleaseIdent::Num(n),
            Err(_) => PreReleaseIdent::Str(ident.to_owned()),
        })
        .collect();
    Some((stability, numbers, pre_idents))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let exists = fs::exists("surgeon.json").unwrap_or(false);
    let mut client = DownloadClient::new().await;

    match &cli.command {
        Commands::Init(args) => {
            if exists {
                // TODO: allow updates
                println!("Already configured");
                return;
            }
            client.set_metadata(DownloadClientMetadata {
                modloader: args.modloader,
                mc_version: args.mc_version.clone(),
                channel: args.channel,
                downloads: Vec::new(),
            });
            if let Err(e) = client.save_metadata().await {
                eprintln!("Failed to save surgeon.json: {e}");
                return;
            }
            let result = match client.metadata.modloader {
                Modloader::Paper => client.download_paper().await,
                Modloader::Fabric => client.download_fabric().await,
                Modloader::NeoForge => client.download_neoforge().await,
            };
            if let Err(e) = result {
                eprintln!("Failed to download server: {e}");
                return;
            }
            println!("Initialized!");
        }
        Commands::Download(args) => {
            if !exists {
                println!("Please use `init` first!");
                return;
            }
            let mut resolved: Vec<ProjectVersion> = Vec::new();
            for id in &args.id {
                resolved = client
                    .download_with_deps(
                        id,
                        &args.game_versions,
                        args.channel,
                        false,
                        Some(resolved),
                    )
                    .await;
                if let Err(e) = client.save_metadata().await {
                    eprintln!("Failed to save surgeon.json: {e}");
                }
            }
            println!("Resolved {} project(s)", resolved.len());
        }
        Commands::Update => {
            if !exists {
                println!("Please use `init` first!");
                return;
            }
            let before: Vec<(String, String, bool)> = client
                .metadata
                .downloads
                .iter()
                .map(|d| (d.project_id.clone(), d.version_id.clone(), d.as_dependency))
                .collect();

            let mut resolved: Vec<ProjectVersion> = Vec::new();
            for (project_id, _, as_dependency) in &before {
                resolved = client
                    .download_with_deps(project_id, &[], None, *as_dependency, Some(resolved))
                    .await;
                if let Err(e) = client.save_metadata().await {
                    eprintln!("Failed to save surgeon.json: {e}");
                }
            }

            let updated = before
                .iter()
                .filter(|(project_id, version_id, _)| {
                    client
                        .metadata
                        .downloads
                        .iter()
                        .any(|d| &d.project_id == project_id && &d.version_id != version_id)
                })
                .count();
            let added = client.metadata.downloads.len() - before.len();
            if updated == 0 && added == 0 {
                println!("Everything is up to date");
            } else {
                println!("Updated {updated} project(s), added {added} new dependency(s)");
            }

            // TODO: log orphans
        }
    }
}
