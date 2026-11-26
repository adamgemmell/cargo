use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io;

use anyhow::Context as _;
use cargo_util::{collections::HashMap, paths};
use cargo_util_schemas::manifest::PackageName;
use semver::Version;

use crate::{
    CargoResult, GlobalContext,
    sources::{
        IndexSummary, PathSource,
        source::{MaybePackage, QueryKind, Source},
    },
    util::{VersionReqMatchMode, cache_lock::CacheLockMode, internal},
    workspace::{Dependency, Manifest, Package, PackageId, SourceId, Summary},
};

const BUILTIN_VERSION: Version = Version::new(0, 0, 0);

/// Represents standard library packages built into the toolchain.
///
/// Because Cargo cannot rely on the `rust-src` component being installed, empty packages are
/// written as stubs in Cargo home when downloaded. These packages should not be actually built.
pub struct BuiltinSource<'gctx> {
    /// The unique identifier for this source
    source_id: SourceId,
    gctx: &'gctx GlobalContext,
    /// Cached packages
    packages: RefCell<Option<HashMap<PackageId, Package>>>,
}

impl<'gctx> BuiltinSource<'gctx> {
    pub fn new(source_id: SourceId, gctx: &'gctx GlobalContext) -> CargoResult<Self> {
        Ok(Self {
            source_id,
            gctx,
            packages: Default::default(),
        })
    }

    fn load_cached_stubs(&self, source_id: SourceId, gctx: &GlobalContext) -> CargoResult<()> {
        if self.packages.borrow().is_some() {
            return Ok(());
        }

        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        let root = gctx.builtins_path();
        let root = gctx.assert_package_cache_locked(CacheLockMode::DownloadExclusive, &root);
        let mut packages = HashMap::default();
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                *self.packages.borrow_mut() = Some(packages);
                return Ok(());
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read builtin package cache at `{}`",
                        root.display()
                    )
                });
            }
        };

        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            if !dir.join("Cargo.toml").is_file() || !dir.join("src/lib.rs").is_file() {
                continue;
            }

            let pkg = PathSource::new(&dir, source_id, gctx).root_package()?;
            packages.insert(pkg.package_id(), pkg);
        }
        *self.packages.borrow_mut() = Some(packages);

        Ok(())
    }

    /// Creates a synthetic package on the filesystem
    fn generate_stub(&self, id: PackageId) -> CargoResult<Package> {
        let name: PackageName = id.name().as_str().parse()?;

        let dir = self
            .gctx
            .builtins_path()
            .join(&format!("{name}-{}", id.version()));
        let dir = self
            .gctx
            .assert_package_cache_locked(CacheLockMode::DownloadExclusive, &dir);

        let manifest = Manifest::synthetic(id, dir.join("src/lib.rs"))?;
        let pkg = Package::new(manifest, &dir.join("Cargo.toml"));

        let lib_path = dir.join("src/lib.rs");
        if !lib_path.try_exists()? {
            paths::create_dir_all(dir.join("src"))?;
            paths::write_atomic(lib_path, "")?;
        }
        if !pkg.manifest_path().try_exists()? {
            let contents = pkg.manifest().to_normalized_contents()?;
            // Write the manifest last so an interrupted write can be retried.
            paths::write_atomic(pkg.manifest_path(), contents)?;
        }

        Ok(pkg)
    }
}

#[async_trait::async_trait(?Send)]
impl Source for BuiltinSource<'_> {
    /// All builtin dependencies are opaque, so this will return a summary without any dependencies when queried
    async fn query(
        &self,
        dep: &Dependency,
        _kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> CargoResult<()> {
        if !dep.source_id().is_builtin() {
            // Not our job
            return Ok(());
        }
        let id = PackageId::new(dep.package_name(), BUILTIN_VERSION, self.source_id);

        if dep.matches_id(id, VersionReqMatchMode::Default) {
            let summary = Summary::new(id, Vec::new(), &BTreeMap::new(), None::<String>, None)?;
            f(IndexSummary::Candidate(summary));
        }
        Ok(())
    }

    fn supports_checksums(&self) -> bool {
        false
    }

    fn requires_precise(&self) -> bool {
        false
    }

    fn source_id(&self) -> SourceId {
        self.source_id
    }

    async fn download(&self, id: PackageId) -> CargoResult<MaybePackage> {
        if id.source_id() != self.source_id || *id.version() != BUILTIN_VERSION {
            return Err(internal(format!("failed to find {} in builtin source", id)));
        }
        self.load_cached_stubs(self.source_id, self.gctx)?;

        let mut packages = self.packages.borrow_mut();
        let pkg = match packages.as_mut().unwrap().entry(id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(self.generate_stub(id)?),
        };
        Ok(MaybePackage::Ready(pkg.clone()))
    }

    async fn finish_download(&self, _id: PackageId, _data: Vec<u8>) -> CargoResult<Package> {
        panic!("no download should have started")
    }

    fn fingerprint(&self, _pkg: &Package) -> CargoResult<String> {
        Err(internal(format!("no fingerprint")))
    }

    fn describe(&self) -> String {
        self.source_id.to_string()
    }

    fn invalidate_cache(&self) {
        // Stubs are immutable
    }

    fn set_quiet(&mut self, _quiet: bool) {}
}
