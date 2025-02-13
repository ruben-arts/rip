use crate::types::{Artifact, NormalizedPackageName, SDistFilename, SDistFormat};
use crate::types::{WheelCoreMetaDataError, WheelCoreMetadata};
use crate::utils::ReadAndSeek;
use flate2::read::GzDecoder;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use serde::Serialize;
use std::ffi::OsStr;
use std::io::{ErrorKind, Read, Seek};
use std::path::{Path, PathBuf};
use tar::Archive;

/// Represents a source distribution artifact.
pub struct SDist {
    /// Name of the source distribution
    name: SDistFilename,

    /// Source dist archive
    file: Mutex<Box<dyn ReadAndSeek + Send>>,
}

#[derive(Debug, Serialize)]
pub struct BuildSystem {
    backend_path: Vec<PathBuf>,
    build_backend: String,
}

#[derive(thiserror::Error, Debug)]
pub enum SDistError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("No PKG-INFO found in archive")]
    NoPkgInfoFound,

    #[error("No pyproject.toml found in archive")]
    NoPyProjectTomlFound,

    #[error("Could not parse pyproject.toml")]
    PyProjectTomlParseError(String),

    #[error("Could not parse metadata")]
    WheelCoreMetaDataError(#[from] WheelCoreMetaDataError),
}

impl SDist {
    /// Create this struct from a path
    #[allow(dead_code)]
    pub fn from_path(
        path: &Path,
        normalized_package_name: &NormalizedPackageName,
    ) -> miette::Result<Self> {
        let file_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| miette::miette!("path does not contain a filename"))?;
        let name =
            SDistFilename::from_filename(file_name, normalized_package_name).into_diagnostic()?;
        let bytes = std::fs::File::open(path).into_diagnostic()?;
        Self::new(name, Box::new(bytes))
    }

    /// Find entry in tar archive
    fn find_entry(&self, name: impl AsRef<str>) -> std::io::Result<Option<Vec<u8>>> {
        let mut lock = self.file.lock();
        let mut archive = generic_archive_reader(&mut lock, self.name.format)?;

        // Loop over entries
        for entry in archive.entries()? {
            let mut entry = entry?;

            // Find name in archive and return this
            if entry.path()?.ends_with(name.as_ref()) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Read .PKG-INFO from the archive
    pub fn read_package_info(&self) -> Result<(Vec<u8>, WheelCoreMetadata), SDistError> {
        if let Some(bytes) = self.find_entry("PKG-INFO")? {
            let metadata = WheelCoreMetadata::try_from(bytes.as_slice())?;

            Ok((bytes, metadata))
        } else {
            Err(SDistError::NoPkgInfoFound)
        }
    }

    /// Read the build system info from the pyproject.toml
    #[allow(dead_code)]
    pub fn read_build_info(&self) -> Result<pyproject_toml::BuildSystem, SDistError> {
        if let Some(bytes) = self.find_entry("pyproject.toml")? {
            let source = String::from_utf8(bytes).map_err(|e| {
                SDistError::PyProjectTomlParseError(format!(
                    "could not parse pyproject.toml (bad encoding): {}",
                    e
                ))
            })?;
            let project = pyproject_toml::PyProjectToml::new(&source).map_err(|e| {
                SDistError::PyProjectTomlParseError(format!(
                    "could not parse pyproject.toml (bad toml): {}",
                    e
                ))
            })?;
            Ok(project
                .build_system
                .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "no build-system found"))?)
        } else {
            Err(SDistError::NoPyProjectTomlFound)
        }
    }

    /// Extract the contents of the sdist archive to the given directory
    pub fn extract_to(&self, work_dir: &Path) -> std::io::Result<()> {
        let mut lock = self.file.lock();
        let mut archive = generic_archive_reader(&mut lock, self.name.format)?;
        archive.unpack(work_dir)?;
        Ok(())
    }

    /// Checks if this artifact implements PEP 643
    /// and returns the metadata if it does
    pub fn pep643_metadata(&self) -> Option<(Vec<u8>, WheelCoreMetadata)> {
        // Assume we have a PKG-INFO
        let (bytes, metadata) = self.read_package_info().ok()?;
        if metadata.metadata_version.implements_pep643() {
            Some((bytes, metadata))
        } else {
            None
        }
    }
}

impl Artifact for SDist {
    type Name = SDistFilename;

    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            file: Mutex::new(bytes),
        })
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

enum RawAndGzReader<'a> {
    Raw(&'a mut Box<dyn ReadAndSeek + Send>),
    Gz(GzDecoder<&'a mut Box<dyn ReadAndSeek + Send>>),
}

impl<'a> Read for RawAndGzReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Raw(r) => r.read(buf),
            Self::Gz(r) => r.read(buf),
        }
    }
}

fn generic_archive_reader(
    file: &mut Box<dyn ReadAndSeek + Send>,
    format: SDistFormat,
) -> std::io::Result<Archive<RawAndGzReader>> {
    file.rewind()?;

    match format {
        SDistFormat::TarGz => {
            let bytes = GzDecoder::new(file);
            Ok(Archive::new(RawAndGzReader::Gz(bytes)))
        }
        SDistFormat::Tar => Ok(Archive::new(RawAndGzReader::Raw(file))),
        _ => Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "sdist archive format currently unsupported (only tar and tar.gz are supported)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::artifacts::SDist;
    use crate::python_env::Pep508EnvMakers;
    use crate::wheel_builder::WheelBuilder;
    use crate::{index::PackageDb, resolve::ResolveOptions};
    use insta::{assert_debug_snapshot, assert_ron_snapshot};
    use std::path::Path;
    use tempfile::TempDir;

    fn get_package_db() -> (PackageDb, TempDir) {
        let tempdir = tempfile::tempdir().unwrap();
        (
            PackageDb::new(
                Default::default(),
                &[url::Url::parse("https://pypi.org/simple/").unwrap()],
                tempdir.path(),
            )
            .unwrap(),
            tempdir,
        )
    }

    #[tokio::test]
    pub async fn correct_metadata_fake_flask() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/fake-flask-3.0.0.tar.gz");

        let sdist = SDist::from_path(&path, &"fake-flask".parse().unwrap()).unwrap();
        // Should not fail as it is a valid PKG-INFO
        // and considered reliable
        let _package_db = get_package_db();
        sdist.pep643_metadata().unwrap();
    }

    #[test]
    pub fn read_rich_build_info() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = super::SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let build_system = sdist.read_build_info().unwrap();

        assert_ron_snapshot!(build_system, @r###"
        BuildSystem(
          requires: [
            "poetry-core >=1.0.0",
          ],
          r#build-backend: Some("poetry.core.masonry.api"),
          r#backend-path: None,
        )
        "###);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn sdist_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Pep508EnvMakers::from_env().await.unwrap();
        let resolve_options = ResolveOptions::default();
        let wheel_builder = WheelBuilder::new(&package_db.0, &env_markers, None, &resolve_options);

        let result = wheel_builder.get_sdist_metadata(&sdist).await.unwrap();

        assert_debug_snapshot!(result.1);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_with_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Pep508EnvMakers::from_env().await.unwrap();
        let resolve_options = ResolveOptions::default();
        let wheel_builder = WheelBuilder::new(&package_db.0, &env_markers, None, &resolve_options);

        // Build the wheel
        wheel_builder.get_sdist_metadata(&sdist).await.unwrap();
        let result = wheel_builder.build_wheel(&sdist).await.unwrap();

        // Try to re-open the wheel
        let wheel = crate::artifacts::Wheel::from_path(&result, &"rich".parse().unwrap()).unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        assert_debug_snapshot!(metadata);
    }
    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_no_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Pep508EnvMakers::from_env().await.unwrap();
        let resolve_options = ResolveOptions::default();
        let wheel_builder = WheelBuilder::new(&package_db.0, &env_markers, None, &resolve_options);

        // Build the wheel
        let result = wheel_builder.build_wheel(&sdist).await.unwrap();

        // Try to re-open the wheel
        let wheel = crate::artifacts::Wheel::from_path(&result, &"rich".parse().unwrap()).unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        assert_debug_snapshot!(metadata);
    }
}
