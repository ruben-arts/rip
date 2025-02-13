use crate::artifacts::wheel::UnpackWheelOptions;
use crate::artifacts::{SDist, Wheel};
use crate::index::PackageDb;
use crate::python_env::{PythonLocation, VEnv, WheelTags};
use crate::resolve::{resolve, PinnedPackage, ResolveOptions};
use crate::types::Artifact;
use crate::wheel_builder::{build_requirements, WheelBuildError};
use pep508_rs::{MarkerEnvironment, Requirement};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./wheel_builder_frontend.py");
/// A build environment for building wheels
/// This struct contains the virtualenv and everything that is needed
/// to execute the PEP517 build backend hools
#[derive(Debug)]
pub(crate) struct BuildEnvironment<'db> {
    work_dir: tempfile::TempDir,
    package_dir: PathBuf,
    #[allow(dead_code)]
    build_system: pyproject_toml::BuildSystem,
    entry_point: String,
    build_requirements: Vec<Requirement>,
    resolved_wheels: Vec<PinnedPackage<'db>>,
    venv: VEnv,
}

impl<'db> BuildEnvironment<'db> {
    /// Extract the wheel and write the build_frontend.py to the work folder
    pub(crate) fn install_build_files(&self, sdist: &SDist) -> std::io::Result<()> {
        // Extract the sdist to the work folder
        sdist.extract_to(self.work_dir.path())?;
        // Write the python frontend to the work folder
        std::fs::write(
            self.work_dir.path().join("build_frontend.py"),
            BUILD_FRONTEND_PY,
        )
    }

    pub(crate) fn work_dir(&self) -> &Path {
        self.work_dir.path()
    }

    /// Get the extra requirements and combine these to the existing requirements
    /// This uses the `GetRequiresForBuildWheel` entry point of the build backend.
    /// this might not be available for all build backends.
    /// and it can also return an empty list of requirements.
    fn get_extra_requirements(&self) -> Result<HashSet<Requirement>, WheelBuildError> {
        let output = self.run_command("GetRequiresForBuildWheel").map_err(|e| {
            WheelBuildError::CouldNotRunCommand("GetRequiresForBuildWheel".into(), e)
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stderr.to_string()));
        }

        // The extra requirements are stored in a file called extra_requirements.json
        let extra_requirements_json =
            std::fs::read_to_string(self.work_dir.path().join("extra_requirements.json"))?;
        let extra_requirements: Vec<String> = serde_json::from_str(&extra_requirements_json)?;

        Ok(HashSet::<Requirement>::from_iter(
            extra_requirements
                .iter()
                .map(|s| Requirement::from_str(s).expect("...")),
        ))
    }

    /// Install extra requirements into the venv, if any extra were found
    /// If the extra requirements are already installed, this will do nothing
    /// for that requirement.
    pub(crate) async fn install_extra_requirements(
        &self,
        package_db: &'db PackageDb,
        env_markers: &MarkerEnvironment,
        wheel_tags: Option<&WheelTags>,
        resolve_options: &ResolveOptions,
    ) -> Result<(), WheelBuildError> {
        // Get extra requirements if any
        let extra_requirements = self.get_extra_requirements()?;

        // Combine previous requirements with extra requirements
        let combined_requirements = HashSet::from_iter(self.build_requirements.iter().cloned())
            .union(&extra_requirements)
            .cloned()
            .collect::<Vec<_>>();

        // Install extra requirements if any new ones were foujnd
        if !extra_requirements.is_empty()
            && self.build_requirements.len() != combined_requirements.len()
        {
            let locked_packages = HashMap::default();
            // Todo: use the previous resolve for the favored packages?
            let favored_packages = HashMap::default();
            let all_requirements = combined_requirements.to_vec();
            let extra_resolved_wheels = resolve(
                package_db,
                all_requirements.iter(),
                env_markers,
                wheel_tags,
                locked_packages,
                favored_packages,
                resolve_options,
            )
            .await
            .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(all_requirements))?;

            // install extra wheels
            for package_info in extra_resolved_wheels {
                if self.resolved_wheels.contains(&package_info) {
                    continue;
                }
                tracing::info!(
                    "installing extra requirements: {} - {}",
                    package_info.name,
                    package_info.version
                );
                let artifact_info = package_info.artifacts.first().unwrap();
                let artifact = package_db
                    .get_artifact::<Wheel>(artifact_info)
                    .await
                    .expect("could not get artifact");

                self.venv
                    .install_wheel(&artifact, &UnpackWheelOptions::default())?;
            }
        }
        Ok(())
    }

    /// Run a command in the build environment
    pub(crate) fn run_command(&self, stage: &str) -> std::io::Result<Output> {
        // three args: cache.folder, goal
        Command::new(self.venv.python_executable())
            .current_dir(&self.package_dir)
            .arg(self.work_dir.path().join("build_frontend.py"))
            .arg(self.work_dir.path())
            .arg(&self.entry_point)
            .arg(stage)
            .output()
    }

    /// Setup the build environment so that we can build a wheel from an sdist
    pub(crate) async fn setup(
        sdist: &SDist,
        package_db: &'db PackageDb,
        env_markers: &MarkerEnvironment,
        wheel_tags: Option<&WheelTags>,
        resolve_options: &ResolveOptions,
    ) -> Result<BuildEnvironment<'db>, WheelBuildError> {
        // Setup a work directory and a new env dir
        let work_dir = tempfile::tempdir().unwrap();
        let venv = VEnv::create(&work_dir.path().join("venv"), PythonLocation::System).unwrap();

        // Find the build system
        let build_system =
            sdist
                .read_build_info()
                .unwrap_or_else(|_| pyproject_toml::BuildSystem {
                    requires: Vec::new(),
                    build_backend: None,
                    backend_path: None,
                });
        // Find the build requirements
        let build_requirements = build_requirements(&build_system);
        // Resolve the build environment
        let resolved_wheels = resolve(
            package_db,
            build_requirements.iter(),
            env_markers,
            wheel_tags,
            HashMap::default(),
            HashMap::default(),
            resolve_options,
        )
        .await
        .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(build_requirements.to_vec()))?;

        // Install into venv
        for package_info in resolved_wheels.iter() {
            let artifact_info = package_info.artifacts.first().unwrap();
            let artifact = package_db
                .get_artifact::<Wheel>(artifact_info)
                .await
                .map_err(|_| WheelBuildError::CouldNotGetArtifact)?;

            venv.install_wheel(
                &artifact,
                &UnpackWheelOptions {
                    installer: None,
                    ..Default::default()
                },
            )?;
        }

        const DEFAULT_BUILD_BACKEND: &str = "setuptools.build_meta:__legacy__";
        let entry_point = build_system
            .build_backend
            .clone()
            .unwrap_or_else(|| DEFAULT_BUILD_BACKEND.to_string());

        // Package dir for the package we need to build
        let package_dir = work_dir.path().join(format!(
            "{}-{}",
            sdist.name().distribution.as_source_str(),
            sdist.name().version
        ));

        Ok(BuildEnvironment {
            work_dir,
            package_dir,
            build_system,
            build_requirements,
            entry_point,
            resolved_wheels,
            venv,
        })
    }
}
