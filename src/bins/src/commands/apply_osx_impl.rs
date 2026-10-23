use crate::{dialogs, shared};
use anyhow::{bail, Result};
use std::{fs, io, path::{Path, PathBuf}, process::Command};
use velopack::{bundle, locator::VelopackLocator};


// Keep extraction beside a writable app so replacement does not rename across volumes.
// Protected installation folders retain the existing consented elevation path.
fn staging_paths(root: &Path, cache_new: &Path, cache_old: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let parent = root.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "App has no parent folder"))?;
    let suffix = cache_new.file_name().unwrap().to_string_lossy();
    let new_path = parent.join(format!(".velopack-{}-new", suffix));
    let old_path = parent.join(format!(".velopack-{}-old", suffix));
    match fs::create_dir(&new_path) {
        Ok(()) => Ok((new_path, old_path)),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(libc::EROFS) => {
            fs::create_dir_all(cache_new)?;
            Ok((cache_new.to_path_buf(), cache_old.to_path_buf()))
        }
        Err(e) => Err(e),
    }
}

// Reuse the native swap used on the fork's development branch. Its fallback
// also works on filesystems that do not implement RENAME_SWAP.
fn atomic_swap(a: &Path, b: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename_with_restore<F>(root: &Path, old: &Path, new: &Path, mut rename: F) -> io::Result<()>
where F: FnMut(&Path, &Path) -> io::Result<()> {
    rename(root, old)?;
    if let Err(error) = rename(new, root) {
        if let Err(restore) = rename(old, root) {
            return Err(io::Error::new(restore.kind(), format!(
                "Replacement failed ({}); could not restore the app ({}). Original retained at {}",
                error, restore, old.display())));
        }
        return Err(error);
    }
    Ok(())
}

fn replace_bundle(root: &Path, old: &Path, new: &Path) -> io::Result<()> {
    match atomic_swap(root, new) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(e),
        Err(_) => rename_with_restore(root, old, new, |a, b| fs::rename(a, b)),
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn elevated_command(root: &Path, old: &Path, new: &Path) -> String {
    // /bin/mv handles different volumes on the existing elevated path.
    format!(
        "/bin/mv -f {root} {old} && {{ if /bin/mv -f {new} {root}; then /bin/rm -rf {old}; else result=$?; /bin/rm -rf {root} && /bin/mv -f {old} {root}; exit $result; fi; }}",
        root = shell_path(root), old = shell_path(old), new = shell_path(new))
}

fn elevated_script(root: &Path, old: &Path, new: &Path) -> String {
    let command = elevated_command(root, old, new);
    format!("do shell script \"{}\" with administrator privileges", command.replace('\\', "\\\\").replace('\"', "\\\"").replace('\n', "\\n").replace('\r', "\\r"))
}

fn cleanup_staging(new: &Path, old: &Path, succeeded: bool) {
    let _ = fs::remove_dir_all(new);
    // Never delete the only old copy after a failed restore.
    if succeeded { let _ = fs::remove_dir_all(old); }
}

pub fn apply_package_impl(locator: &VelopackLocator, pkg: &PathBuf, _hook_mode: super::HookRunMode) -> Result<VelopackLocator> {
    let _mutex = locator.try_get_exclusive_lock()?;
    let root_path = locator.get_root_dir();
    let mut bundle = bundle::load_bundle_from_file(pkg).map_err(|e| {
        warn!("Deleting package {:?} to prevent update loop: {}", pkg, e);
        let _ = fs::remove_file(pkg);
        e
    })?;
    let manifest = bundle.read_manifest().map_err(|e| {
        warn!("Deleting package {:?} to prevent update loop: {}", pkg, e);
        let _ = fs::remove_file(pkg);
        e
    })?;
    let new_locator = locator.clone_self_with_new_manifest(&manifest);

    let (tmp_path_new, tmp_path_old) = staging_paths(
        &root_path, &locator.get_temp_dir_rand16(), &locator.get_temp_dir_rand16())?;

    // show progress dialog
    let reporter = dialogs::progress::show_apply_progress(&manifest.title, &manifest.version.to_string());

    let action: Result<()> = (|| {
        // 1. extract the bundle to a temp dir
        fs::create_dir_all(&tmp_path_new)?;
        info!("Extracting bundle to {:?}", &tmp_path_new);
        bundle.extract_lib_contents_to_path(&tmp_path_new, |p| reporter.set_progress(p))?;

        // 2. attempt to replace the current bundle with the new one
        reporter.set_indeterminate();
        let result: Result<()> = (|| {
            info!("Replacing bundle at {:?}", &root_path);
            replace_bundle(&root_path, &tmp_path_old, &tmp_path_new)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                info!("Bundle extracted successfully to {:?}", &root_path);
                Ok(())
            }
            Err(e) => {
                // 3. if fails for permission error, try again escalated via osascript
                if shared::is_error_permission_denied(&e) {
                    error!(
                        "A permissions error occurred ({}), will attempt to elevate permissions and try again...",
                        e
                    );
                    dialogs::ask_user_to_elevate(&manifest.title, &manifest.version.to_string())?;
                    let script = elevated_script(&root_path, &tmp_path_old, &tmp_path_new);
                    info!("Running elevated process via osascript: {}", script);
                    let output = Command::new("osascript").arg("-e").arg(&script).status()?;
                    if output.success() {
                        info!("Bundle applied successfully via osascript.");
                        Ok(())
                    } else {
                        bail!("elevated process failed: exited with code: {}", output);
                    }
                } else {
                    bail!("Failed to extract bundle ({})", e);
                }
            }
        }
    })();
    reporter.close();
    cleanup_staging(&tmp_path_new, &tmp_path_old, action.is_ok());
    action?;
    Ok(new_locator)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::symlink, fs::MetadataExt};

    fn fixture(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("blackbird_mac_update_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn make_app(path: &Path, text: &str) {
        fs::create_dir_all(path.join("Contents")).unwrap();
        fs::write(path.join("Contents/Info.plist"), text).unwrap();
        symlink("Info.plist", path.join("Contents/link")).unwrap();
    }

    #[test]
    fn swaps_complete_apps_and_preserves_links() {
        let base = fixture("swap");
        let (root, old, new) = (base.join("App.app"), base.join("old"), base.join("new"));
        make_app(&root, "old"); make_app(&new, "new");
        replace_bundle(&root, &old, &new).unwrap();
        assert_eq!(fs::read_to_string(root.join("Contents/link")).unwrap(), "new");
        cleanup_staging(&new, &old, true);
        assert!(!old.exists() && !new.exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn fallback_restores_old_app_when_second_move_fails() {
        let base = fixture("restore");
        let (root, old, new) = (base.join("App.app"), base.join("old"), base.join("missing-new"));
        make_app(&root, "old");
        assert!(rename_with_restore(&root, &old, &new, |a,b| fs::rename(a,b)).is_err());
        cleanup_staging(&new, &old, false);
        assert_eq!(fs::read_to_string(root.join("Contents/link")).unwrap(), "old");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cleanup_retains_original_if_restore_itself_is_refused() {
        let base = fixture("retained");
        let (root, old, new) = (base.join("App.app"), base.join("old"), base.join("new"));
        make_app(&root, "old"); make_app(&new, "new");
        let mut moves = 0;
        let failure = rename_with_restore(&root, &old, &new, |a,b| {
            moves += 1;
            if moves > 1 { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "test refusal")); }
            fs::rename(a,b)
        }).unwrap_err();
        cleanup_staging(&new, &old, false);
        assert!(failure.to_string().contains("Original retained at"));
        assert_eq!(fs::read_to_string(old.join("Contents/link")).unwrap(), "old");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn refused_first_move_keeps_original_untouched() {
        let base = fixture("first_move");
        let (root, old, new) = (base.join("App.app"), base.join("old"), base.join("new"));
        make_app(&root, "old"); make_app(&new, "new");
        assert!(rename_with_restore(&root, &old, &new, |_,_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused"))).is_err());
        cleanup_staging(&new, &old, false);
        assert_eq!(fs::read_to_string(root.join("Contents/link")).unwrap(), "old");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn elevated_move_failure_restores_old_app_without_requesting_privileges() {
        let base = fixture("shell_quote");
        let (root, old, new) = (base.join("App ' \" name.app"), base.join("old ' name"), base.join("missing ' new"));
        make_app(&root, "old");
        let status = Command::new("/bin/sh").arg("-c").arg(elevated_command(&root, &old, &new)).status().unwrap();
        assert!(!status.success());
        assert_eq!(fs::read_to_string(root.join("Contents/link")).unwrap(), "old");
        assert!(!old.exists());
        let script = elevated_script(&root, &old, &new);
        assert!(script.starts_with("do shell script \""));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn extraction_failure_does_not_change_installed_app() {
        let base = fixture("extract");
        let root = base.join("App.app"); make_app(&root, "old");
        let (new, old) = staging_paths(&root, &base.join("cache-new"), &base.join("cache-old")).unwrap();
        fs::write(new.join("partial-file"), "incomplete extraction").unwrap();
        cleanup_staging(&new, &old, false);
        assert_eq!(fs::read_to_string(root.join("Contents/link")).unwrap(), "old");
        assert!(!new.exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    #[ignore = "requires an explicitly supplied disposable second volume"]
    fn stages_and_replaces_across_app_and_cache_volumes() {
        let cache = fixture("cross_volume_cache");
        let other = std::env::var_os("BLACKBIRD_TEST_OTHER_VOLUME").expect("CI must supply a disposable second volume");
        let base = PathBuf::from(other).join(format!("blackbird_update_{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        assert_ne!(fs::metadata(&cache).unwrap().dev(), fs::metadata(&base).unwrap().dev());
        let root = base.join("App.app"); make_app(&root, "old");
        let (new, old) = staging_paths(&root, &cache.join("new"), &cache.join("old")).unwrap();
        assert_eq!(new.parent(), root.parent());
        assert_eq!(old.parent(), root.parent());
        make_app(&new, "new");
        replace_bundle(&root, &old, &new).unwrap();
        cleanup_staging(&new, &old, true);
        assert_eq!(fs::read_to_string(root.join("Contents/link")).unwrap(), "new");
        fs::remove_dir_all(base).unwrap(); fs::remove_dir_all(cache).unwrap();
    }
}
