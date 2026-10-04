//! Native copy-on-write workspaces. Ownership and baselines live outside agent checkouts.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
type Result<T> = std::result::Result<T, String>;
const UNSUPPORTED_FILESYSTEM: &str = "Copy-on-write requires APFS on macOS";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub path: String,
    pub source_cwd: String,
    pub project_cwd: String,
    pub session_id: String,
    pub branch: Option<String>,
    pub head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpushed: Option<u64>,
    baseline: String,
    excluded: BTreeSet<String>,
    identity: (u64, u64),
    #[serde(default)]
    git_config_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    removal_path: Option<String>,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn canonical(p: &Path) -> Result<PathBuf> {
    p.canonicalize().map_err(err)
}
fn relative(p: &str) -> Result<()> {
    if p.is_empty()
        || p.contains('\0')
        || Path::new(p)
            .components()
            .any(|x| !matches!(x, Component::Normal(_)))
    {
        return Err("Invalid project-relative path".into());
    }
    Ok(())
}
fn id_valid(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| "Invalid isolation ID".into())
}
#[cfg(unix)]
fn identity(p: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(p).map_err(err)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err("Isolation root was replaced".into());
    }
    Ok((m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn identity(_: &Path) -> Result<(u64, u64)> {
    Err(UNSUPPORTED_FILESYSTEM.into())
}
fn private_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = fs::symlink_metadata(p).map_err(err)?;
        if !m.is_dir() || m.file_type().is_symlink() || m.uid() != unsafe { libc::geteuid() } {
            return Err("Isolation storage must be an owned directory".into());
        }
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).map_err(err)?;
    }
    Ok(())
}
fn write_json(p: &Path, v: &impl Serialize) -> Result<()> {
    let temp = p.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut f = File::options()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(err)?;
    serde_json::to_writer(&mut f, v).map_err(err)?;
    f.sync_all().map_err(err)?;
    fs::rename(temp, p).map_err(err)
}
fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let parent = p.parent().ok_or("Missing registry parent")?;
    let metadata = fs::symlink_metadata(parent).map_err(err)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Registry directory was replaced".into());
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Registry directory has a different owner".into());
        }
        File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)
            .map_err(err)?
    };
    #[cfg(not(unix))]
    let file = File::open(p).map_err(err)?;
    serde_json::from_reader(file).map_err(err)
}
fn git_base(root: &Path) -> Command {
    let mut c = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            c.env_remove(key);
        }
    }
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    c.arg("-C").arg(root).args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "diff.external=",
        "-c",
        "core.attributesFile=/dev/null",
        "-c",
        "core.sshCommand=false",
        "-c",
        "protocol.allow=never",
        "-c",
        "protocol.file.allow=always",
    ]);
    c
}
fn git_command(root: &Path) -> Command {
    let mut c = git_base(root);
    // Git status/reset can invoke clean/smudge/process filters from local attributes.
    // Read names only, then override every executable filter before any file operation.
    if let Ok(output) = git_base(root)
        .args([
            "config",
            "--includes",
            "--local",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\.",
        ])
        .output()
    {
        let mut names = BTreeSet::new();
        for key in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
            if let Ok(key) = std::str::from_utf8(key) {
                if let Some((name, _)) = key.rsplit_once('.') {
                    names.insert(name.to_string());
                }
            }
        }
        for name in names {
            for setting in ["clean=", "smudge=", "process=", "required=false"] {
                c.arg("-c").arg(format!("{name}.{setting}"));
            }
        }
    }
    c
}
fn git(root: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut c = git_command(root);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        c.stdin(Stdio::piped());
    }
    let mut child = c.spawn().map_err(err)?;
    if let Some(bytes) = input {
        let mut stdin = child.stdin.take().ok_or("Git stdin missing")?;
        stdin.write_all(bytes).map_err(err)?;
    }
    let out = child.wait_with_output().map_err(err)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(out.stdout)
}
fn text(root: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(git(root, args, None)?)
        .map(|s| s.trim().to_string())
        .map_err(err)
}
fn repo(p: &Path) -> Result<PathBuf> {
    let root = canonical(p)?;
    let top = text(&root, &["rev-parse", "--show-toplevel"])?;
    if canonical(Path::new(&top))? != root {
        return Err("Select the repository root for copy-on-write".into());
    }
    Ok(root)
}
fn paths(root: &Path) -> Result<BTreeSet<String>> {
    let bytes = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        None,
    )?;
    let mut out = BTreeSet::new();
    for b in bytes.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let p = String::from_utf8(b.to_vec()).map_err(|_| "Non-UTF-8 paths are unsupported")?;
        relative(&p)?;
        out.insert(p);
    }
    Ok(out)
}
fn validate_repo(root: &Path) -> Result<()> {
    text(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !git(root, &["ls-files", "-u"], None)?.is_empty() {
        return Err("Resolve index conflicts before cloning".into());
    }
    if git(root, &["ls-files", "-v", "-z"], None)?
        .split(|b| *b == 0)
        .any(|record| {
            record
                .first()
                .is_some_and(|b| b.is_ascii_lowercase() || *b == b'S')
        })
    {
        return Err("Skip-worktree and assume-unchanged index entries are unsupported; clear their flags before using copy-on-write".into());
    }
    for args in [
        &["config", "--get", "core.sparseCheckout"][..],
        &["config", "--get", "extensions.partialClone"][..],
        &["rev-parse", "--shared-index-path"][..],
    ] {
        if text(root, args).is_ok_and(|s| !s.is_empty() && s != "false") {
            return Err("Sparse, split-index and partial repositories are unsupported".into());
        }
    }
    if text(root, &["rev-parse", "--is-shallow-repository"])? == "true" {
        return Err("Shallow repositories are unsupported".into());
    }
    let common = PathBuf::from(text(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    if common.join("objects/info/alternates").exists() {
        return Err("Git object alternates are unsupported".into());
    }
    let stage = git(root, &["ls-files", "--stage"], None)?;
    if String::from_utf8_lossy(&stage)
        .lines()
        .any(|s| s.starts_with("160000 "))
    {
        return Err("Submodules are unsupported".into());
    }
    Ok(())
}
// Platform clone primitive; filesystem validation lives in check_filesystem.
#[cfg(target_os = "macos")]
fn clone_file(source: &File, parent: &File, name: &std::ffi::CStr) -> Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn fclonefileat(src: i32, dst: i32, name: *const libc::c_char, flags: u32) -> i32;
    }
    if unsafe { fclonefileat(source.as_raw_fd(), parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(format!(
            "Native APFS clone failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn clone_file(_: &File, _: &File, _: &std::ffi::CStr) -> Result<()> {
    Err(UNSUPPORTED_FILESYSTEM.into())
}
#[cfg(unix)]
fn open_dir(p: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(p)
        .map_err(err)
}
fn relative_link(parent: &Path, target: &Path) -> PathBuf {
    let from = parent.components().collect::<Vec<_>>();
    let to = target.components().collect::<Vec<_>>();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path = PathBuf::new();
    for _ in common..from.len() {
        path.push("..");
    }
    for component in &to[common..] {
        path.push(component.as_os_str());
    }
    path
}
#[cfg(unix)]
fn clone_tree(source: &Path, dest: &Path, exclude_git: bool) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    #[allow(clippy::unnecessary_cast)] // stat field widths differ between APFS and Linux.
    fn walk(
        src: &Path,
        dst: &Path,
        root: &Path,
        out: &Path,
        source_fd: &File,
        root_dev: u64,
        exclude_git: bool,
    ) -> Result<()> {
        let dest_fd = open_dir(dst)?;
        for entry in fs::read_dir(src).map_err(err)? {
            let entry = entry.map_err(err)?;
            let name = entry.file_name();
            if name == ".git" {
                if src == root && exclude_git {
                    continue;
                }
                if exclude_git {
                    return Err(format!("Nested Git repository: {}", entry.path().display()));
                }
            }
            use std::os::unix::ffi::OsStrExt;
            let cname = std::ffi::CString::new(name.as_bytes()).map_err(err)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    source_fd.as_raw_fd(),
                    cname.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(err(std::io::Error::last_os_error()));
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_dev as u64 != root_dev {
                return Err("Cross-filesystem content is unsupported".into());
            }
            let target = dst.join(&name);
            let kind = stat.st_mode & libc::S_IFMT;
            if kind == libc::S_IFLNK {
                let mut buffer = vec![0u8; 65536];
                let length = unsafe {
                    libc::readlinkat(
                        source_fd.as_raw_fd(),
                        cname.as_ptr(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if length < 0 {
                    return Err(err(std::io::Error::last_os_error()));
                }
                if length as usize == buffer.len() {
                    return Err("Symlink target is too long".into());
                }
                buffer.truncate(length as usize);
                use std::os::unix::ffi::OsStringExt;
                let link = PathBuf::from(std::ffi::OsString::from_vec(buffer));
                let resolved = canonical(&src.join(&link))?;
                if !resolved.starts_with(root) {
                    return Err(format!("External symlink: {}", entry.path().display()));
                }
                let link = if link.is_absolute() {
                    relative_link(
                        dst.strip_prefix(out).map_err(err)?,
                        resolved.strip_prefix(root).map_err(err)?,
                    )
                } else {
                    link
                };
                std::os::unix::fs::symlink(link, target).map_err(err)?;
            } else if kind == libc::S_IFDIR || kind == libc::S_IFREG {
                let flags = libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | if kind == libc::S_IFDIR {
                        libc::O_DIRECTORY
                    } else {
                        0
                    };
                let fd = unsafe { libc::openat(source_fd.as_raw_fd(), cname.as_ptr(), flags) };
                if fd < 0 {
                    return Err(err(std::io::Error::last_os_error()));
                }
                let file = unsafe { File::from_raw_fd(fd) };
                let before = file.metadata().map_err(err)?;
                if before.ino() != stat.st_ino as u64 || before.dev() != stat.st_dev as u64 {
                    return Err("Source changed while cloning".into());
                }
                if kind == libc::S_IFDIR {
                    private_dir(&target)?;
                    walk(
                        &entry.path(),
                        &target,
                        root,
                        out,
                        &file,
                        root_dev,
                        exclude_git,
                    )?;
                } else {
                    if target.exists() {
                        if file_digest(&entry.path())? != file_digest(&target)? {
                            return Err("Conflicting Git object storage".into());
                        }
                    } else {
                        clone_file(&file, &dest_fd, &cname)?;
                    }
                    fs::set_permissions(&target, fs::Permissions::from_mode(before.mode() & 0o777))
                        .map_err(err)?;
                }
                let after = file.metadata().map_err(err)?;
                if before.len() != after.len()
                    || before.mtime() != after.mtime()
                    || before.mtime_nsec() != after.mtime_nsec()
                    || before.ctime() != after.ctime()
                    || before.ctime_nsec() != after.ctime_nsec()
                {
                    return Err("Source changed while cloning; retry".into());
                }
            } else {
                return Err(format!(
                    "Unsupported special file: {}",
                    entry.path().display()
                ));
            }
        }
        Ok(())
    }
    private_dir(dest)?;
    let fd = open_dir(source)?;
    walk(
        source,
        dest,
        source,
        dest,
        &fd,
        fd.metadata().map_err(err)?.dev(),
        exclude_git,
    )
}
#[cfg(not(unix))]
fn clone_tree(_: &Path, _: &Path, _: bool) -> Result<()> {
    Err(UNSUPPORTED_FILESYSTEM.into())
}
#[cfg(target_os = "macos")]
fn check_filesystem(root: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(root.as_os_str().as_bytes()).map_err(err)?;
    let mut s = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(c.as_ptr(), s.as_mut_ptr()) } != 0 {
        return Err(err(std::io::Error::last_os_error()));
    }
    let s = unsafe { s.assume_init() };
    let name = unsafe { std::ffi::CStr::from_ptr(s.f_fstypename.as_ptr()) };
    if name.to_bytes() != b"apfs" {
        return Err(UNSUPPORTED_FILESYSTEM.into());
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn check_filesystem(_: &Path) -> Result<()> {
    Err(UNSUPPORTED_FILESYSTEM.into())
}
fn capability(cwd: &Path) -> Value {
    let result: Result<()> = (|| {
        let root = repo(cwd)?;
        validate_repo(&root)?;
        check_filesystem(&root)?;
        Ok(())
    })();
    match result {
        Ok(()) => json!({"supported":true}),
        Err(reason) => json!({"supported":false,"reason":reason}),
    }
}
fn init_repo(root: &Path, format: &str) -> Result<()> {
    private_dir(root)?;
    text(
        root,
        &[
            "init",
            "--quiet",
            "--template=",
            &format!("--object-format={format}"),
        ],
    )?;
    Ok(())
}
fn private_git(source: &Path, dest: &Path, head: &str, branch: &str) -> Result<()> {
    let format = text(source, &["rev-parse", "--show-object-format"])?;
    init_repo(dest, &format)?;
    let common = PathBuf::from(text(
        source,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    clone_tree(&common.join("objects"), &dest.join(".git/objects"), false)?;
    copy_git_rules(&common, dest, false)?;
    // Copy refs as Git metadata, never shared worktree registrations or pointers.
    let refs = text(
        source,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    for line in refs.lines() {
        let fields = line.split('\0').collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err("Invalid Git reference metadata".into());
        }
        if fields[2].is_empty() {
            text(dest, &["update-ref", fields[0], fields[1]])?;
        } else {
            text(dest, &["symbolic-ref", fields[0], fields[2]])?;
        }
    }
    text(dest, &["update-ref", &format!("refs/heads/{branch}"), head])?;
    text(
        dest,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
    )?;
    text(dest, &["read-tree", head])?;
    copy_git_configuration(source, dest, branch, false)?;
    Ok(())
}
fn copy_git_rules(common: &Path, dest: &Path, migrate: bool) -> Result<()> {
    #[cfg(not(unix))]
    let _ = (common, dest, migrate);
    // Preserve repository-local ignore/attribute rules without sharing Git metadata.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["exclude", "attributes"] {
            let rule = common.join("info").join(name);
            if migrate
                && dest
                    .join(".git/info")
                    .join(name)
                    .try_exists()
                    .map_err(err)?
            {
                continue;
            }
            match fs::symlink_metadata(&rule) {
                Ok(metadata) => {
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        return Err("Git ignore and attribute rules must be regular files".into());
                    }
                    let directory = dest.join(".git/info");
                    private_dir(&directory)?;
                    clone_file(
                        &open_regular(common, &format!("info/{name}"))?,
                        &open_dir(&directory)?,
                        &std::ffi::CString::new(name).map_err(err)?,
                    )?;
                    fs::set_permissions(
                        directory.join(name),
                        fs::Permissions::from_mode(metadata.permissions().mode() & 0o777),
                    )
                    .map_err(err)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(err(error)),
            }
        }
    }
    Ok(())
}
fn config_reader(root: &Path) -> Command {
    let mut command = Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    command.arg("-C").arg(root);
    command
}
fn config_values(root: &Path, key: &str, local: bool) -> Result<Vec<String>> {
    let mut reader = config_reader(root);
    reader.args(["config", "--includes", "--null"]);
    if local {
        reader.arg("--local");
    }
    let output = reader.args(["--get-all", key]).output().map_err(err)?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err("Cannot read Git configuration".into());
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(err)?
        .split_terminator('\0')
        .map(str::to_string)
        .collect())
}
fn copy_git_configuration(source: &Path, dest: &Path, branch: &str, migrate: bool) -> Result<()> {
    // Metadata-only read: includes are resolved for the originating Git directory;
    // no credential helper, SSH command, hook, or network request runs here.
    let output = config_reader(source).args(["config", "--includes", "--null", "--get-regexp", r"^(user\.(name|email|signingkey)|commit\.gpgsign|tag\.gpgsign|gpg(\.(openpgp|x509|ssh))?\.(format|program|allowedsignersfile|defaultkeycommand)|core\.(sshcommand|autocrlf|eol|safecrlf|filemode|ignorecase|checkstat|trustctime|precomposeunicode|symlinks|excludesfile|attributesfile)|filter\..*\.(clean|smudge|process|required)|credential(\..*)?\.(helper|usehttppath|username)|remote\..*\.(url|pushurl|fetch|tagopt|prune)|branch\..*\.(remote|merge|pushremote|rebase))$"]).output().map_err(err)?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err("Cannot read source Git configuration".into());
    }
    let mut configuration: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for field in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let field =
            std::str::from_utf8(field).map_err(|_| "Non-UTF-8 Git configuration is unsupported")?;
        let (key, value) = field.split_once('\n').ok_or("Invalid Git configuration")?;
        configuration
            .entry(key.to_string())
            .or_default()
            .push(value.to_string());
    }
    let mut remotes = BTreeSet::new();
    for (key, values) in &configuration {
        if key.starts_with(&format!("branch.{branch}.")) {
            continue;
        }
        let auth = key == "core.sshcommand" || key.starts_with("credential.");
        let conversion =
            key.starts_with("filter.") || key.starts_with("core.") || key.starts_with("gpg.");
        if key.starts_with("remote.") && (key.ends_with(".url") || key.ends_with(".pushurl")) {
            let remote = key
                .strip_prefix("remote.")
                .and_then(|name| name.rsplit_once('.').map(|(remote, _)| remote))
                .ok_or("Invalid remote configuration")?;
            if !remotes.insert(remote.to_string()) {
                continue;
            }
            for push in [false, true] {
                let target_key =
                    format!("remote.{remote}.{}", if push { "pushurl" } else { "url" });
                if migrate {
                    let local = config_values(dest, &target_key, true)?;
                    let original = configuration
                        .get(&target_key)
                        .or_else(|| configuration.get(&format!("remote.{remote}.url")));
                    if local.is_empty()
                        || Some(&local) != original
                        || local
                            .iter()
                            .any(|url| url.contains(':') || Path::new(url).is_absolute())
                    {
                        continue;
                    }
                }
                let mut reader = config_reader(source);
                reader.args(["remote", "get-url"]);
                if push {
                    reader.arg("--push");
                }
                let output = reader.args(["--all", remote]).output().map_err(err)?;
                if !output.status.success() {
                    return Err("Cannot resolve source Git remote".into());
                }
                let urls = String::from_utf8(output.stdout).map_err(err)?;
                if migrate {
                    let _ = git(dest, &["config", "--unset-all", &target_key], None);
                }
                for url in urls.lines() {
                    let url = if !url.contains(':') && !Path::new(url).is_absolute() {
                        source.join(url).to_string_lossy().into_owned()
                    } else {
                        url.to_string()
                    };
                    text(dest, &["config", "--add", &target_key, &url])?;
                }
            }
            continue;
        }
        if migrate && !auth && !conversion {
            continue;
        }
        if migrate && !config_values(dest, key, true)?.is_empty() {
            continue;
        }
        if auth || conversion {
            if config_values(dest, key, false)? == *values {
                continue;
            }
            if key.ends_with(".helper") {
                text(dest, &["config", "--add", key, ""])?;
            }
        }
        if key.ends_with(".helper") || key.ends_with(".fetch") {
            for value in values {
                text(dest, &["config", "--add", key, value])?;
            }
        } else if let Some(value) = values.last() {
            let value = if ["core.excludesfile", "core.attributesfile"].contains(&key.as_str()) {
                // --path expands ~ and config-relative path syntax without executing Git helpers.
                let path = config_reader(source)
                    .args(["config", "--includes", "--null", "--path", "--get", key])
                    .output()
                    .map_err(err)?;
                if !path.status.success() {
                    return Err("Cannot resolve Git rule path".into());
                }
                let path = String::from_utf8(path.stdout).map_err(err)?;
                let path = Path::new(path.trim_end_matches('\0'));
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    source.join(path)
                };
                let resolved = path.canonicalize().unwrap_or(path);
                // Repository-owned rules follow the isolated checkout; external
                // rule files remain in their existing user-configured location.
                resolved
                    .strip_prefix(source)
                    .map(|relative| dest.join(relative))
                    .unwrap_or(resolved)
                    .to_string_lossy()
                    .into_owned()
            } else {
                value.clone()
            };
            text(dest, &["config", "--replace-all", key, &value])?;
        }
    }
    Ok(())
}
fn upgrade_git_configuration(store: &Path, w: &mut Workspace) -> Result<()> {
    if w.git_config_version >= 2 {
        return Ok(());
    }
    let root = Path::new(&w.path);
    identity(&root.join(".git"))?;
    let common = text(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if Path::new(&common) != root.join(".git") {
        return Err("Isolation Git directory was replaced".into());
    }
    let source = repo(Path::new(&w.source_cwd))?;
    copy_git_configuration(&source, root, w.branch.as_deref().unwrap_or(""), true)?;
    let common = PathBuf::from(text(
        &source,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    copy_git_rules(&common, root, true)?;
    w.git_config_version = 2;
    write_json(&store.join(&w.id).join("workspace.json"), w)
}
fn checked_file(root: &Path, p: &str) -> Result<PathBuf> {
    relative(p)?;
    let target = root.join(p);
    let mut parent = target.parent().ok_or("Invalid path")?;
    while parent != root {
        if fs::symlink_metadata(parent).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("Symlink ancestor: {p}"));
        }
        parent = parent.parent().ok_or("Path escaped checkout")?;
    }
    Ok(target)
}
#[cfg(unix)]
fn open_regular(root: &Path, relative_path: &str) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    relative(relative_path)?;
    let components: Vec<_> = Path::new(relative_path).components().collect();
    let mut parent = open_dir(root)?;
    for (index, component) in components.iter().enumerate() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(component.as_os_str().as_bytes()).map_err(err)?;
        let directory = index + 1 < components.len();
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | if directory { libc::O_DIRECTORY } else { 0 },
            )
        };
        if fd < 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        parent = unsafe { File::from_raw_fd(fd) };
    }
    if !parent.metadata().map_err(err)?.is_file() {
        return Err("Not a regular file".into());
    }
    Ok(parent)
}
#[cfg(not(unix))]
fn open_regular(root: &Path, p: &str) -> Result<File> {
    File::open(checked_file(root, p)?).map_err(err)
}
fn blob(root: &Path, p: &str) -> Result<Option<(String, String)>> {
    let target = checked_file(root, p)?;
    let m = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(e)),
    };
    if m.file_type().is_symlink() {
        let resolved = canonical(&target)?;
        if !resolved.starts_with(root) {
            return Err(format!("External symlink: {p}"));
        }
        let link = fs::read_link(&target).map_err(err)?;
        let link = if link.is_absolute() {
            relative_link(
                target
                    .parent()
                    .ok_or("Missing symlink parent")?
                    .strip_prefix(root)
                    .map_err(err)?,
                resolved.strip_prefix(root).map_err(err)?,
            )
        } else {
            link
        };
        return Ok(Some((
            "120000".into(),
            link.to_str().ok_or("Non-UTF-8 symlink target")?.to_string(),
        )));
    }
    if !m.is_file() {
        return Err(format!("Unsupported changed file: {p}"));
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o111 != 0 {
            "100755"
        } else {
            "100644"
        }
    };
    #[cfg(not(unix))]
    let mode = "100644";
    Ok(Some((mode.into(), target.to_string_lossy().into_owned())))
}
fn tree(
    objects: &Path,
    root: &Path,
    eligible: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
) -> Result<String> {
    let index = objects.join(format!("index-{}", uuid::Uuid::new_v4()));
    let mut c = git_base(objects);
    c.env("GIT_INDEX_FILE", &index)
        .args(["read-tree", "--empty"]);
    if !c.output().map_err(err)?.status.success() {
        return Err("Cannot initialize snapshot index".into());
    }
    let result = (|| {
        let mut entries = Vec::new();
        for p in eligible.difference(excluded) {
            let Some((mode, data)) = blob(root, p)? else {
                continue;
            };
            let oid = if mode == "120000" {
                String::from_utf8(git(
                    objects,
                    &["hash-object", "-w", "--stdin"],
                    Some(data.as_bytes()),
                )?)
                .map_err(err)?
                .trim()
                .to_string()
            } else {
                let mut c = git_base(objects);
                c.args(["hash-object", "-w", "--stdin"])
                    .stdin(Stdio::from(open_regular(root, p)?));
                let out = c.output().map_err(err)?;
                if !out.status.success() {
                    return Err("Cannot hash file".into());
                }
                String::from_utf8(out.stdout)
                    .map_err(err)?
                    .trim()
                    .to_string()
            };
            entries.extend_from_slice(format!("{mode} {oid}\t{p}\0").as_bytes());
        }
        let mut child = git_base(objects)
            .env("GIT_INDEX_FILE", &index)
            .args(["update-index", "-z", "--index-info"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(err)?;
        child
            .stdin
            .take()
            .ok_or("Missing index input")?
            .write_all(&entries)
            .map_err(err)?;
        let updated = child.wait_with_output().map_err(err)?;
        if !updated.status.success() {
            return Err(String::from_utf8_lossy(&updated.stderr).into_owned());
        }
        let out = git_base(objects)
            .env("GIT_INDEX_FILE", &index)
            .arg("write-tree")
            .output()
            .map_err(err)?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).to_string());
        }
        String::from_utf8(out.stdout)
            .map(|s| s.trim().to_string())
            .map_err(err)
    })();
    let _ = fs::remove_file(index);
    result
}
#[cfg(unix)]
fn file_digest(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let mut f = File::open(p).map_err(err)?;
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b).map_err(err)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn fingerprint(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, p: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        for e in fs::read_dir(p).map_err(err)? {
            let e = e.map_err(err)?;
            if p == root && e.file_name() == ".git" {
                continue;
            }
            let path = e.path();
            let m = fs::symlink_metadata(&path).map_err(err)?;
            let rel = path
                .strip_prefix(root)
                .map_err(err)?
                .to_str()
                .ok_or("Non-UTF-8 path")?
                .to_string();
            if m.is_dir() {
                walk(root, &path, out)?;
            } else if m.is_file() {
                let mut h = Sha256::new();
                let mut f = open_regular(root, &rel)?;
                let mut buf = [0; 65536];
                loop {
                    let n = f.read(&mut buf).map_err(err)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o111
                };
                #[cfg(not(unix))]
                let executable = 0;
                out.insert(rel, format!("{executable}:{:x}", h.finalize()));
            } else if m.file_type().is_symlink() {
                let (_, link) = blob(root, &rel)?.ok_or("Symlink disappeared during capture")?;
                out.insert(rel, format!("link:{link}"));
            } else {
                return Err("Unsupported special file".into());
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}
fn load(store: &Path, cwd: &Path, id: &str) -> Result<Workspace> {
    id_valid(id)?;
    let mut w: Workspace = read_json(&store.join(id).join("workspace.json"))?;
    let cwd = canonical(cwd)?;
    if cwd != Path::new(&w.project_cwd)
        && cwd != Path::new(&w.source_cwd)
        && cwd != Path::new(&w.path)
    {
        return Err("Isolation belongs to another project".into());
    }
    let root = Path::new(&w.path).to_path_buf();
    if identity(&root)? != w.identity {
        return Err("Isolation root was replaced".into());
    }
    upgrade_git_configuration(store, &mut w)?;
    Ok(w)
}
fn snapshot_directory(store: &Path, path: &Path, id: &str) -> Result<PathBuf> {
    id_valid(id)?;
    let parent = path
        .parent()
        .ok_or("Invalid isolation directory")?
        .join(".baselines");
    let baseline = parent.join(id);
    // Existing pre-release records used app-data snapshots; keep them recoverable.
    let legacy = store.join(id).join("baseline");
    if !baseline.exists() && legacy.exists() {
        identity(&legacy)?;
        return Ok(legacy);
    }
    private_dir(&parent)?;
    if baseline.exists() {
        identity(&baseline)?;
    }
    Ok(baseline)
}
fn resolve_base(source: &Path, base: &str) -> Result<String> {
    if base == "HEAD" {
        return text(source, &["rev-parse", "--verify", "HEAD^{commit}"]);
    }
    if base.is_empty() || base.len() > 1024 || base.starts_with('-') || base.contains('\0') {
        return Err("Choose an available base branch".into());
    }
    let refs = text(
        source,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let chosen = refs
        .lines()
        .find(|reference| {
            *reference == base
                || *reference == format!("refs/heads/{base}")
                || *reference == format!("refs/remotes/{base}")
        })
        .ok_or("Choose an available base branch")?;
    text(
        source,
        &["rev-parse", "--verify", &format!("{chosen}^{{commit}}")],
    )
}
fn create(
    store: &Path,
    cwd: &Path,
    session: &str,
    project: Option<&Path>,
    base: Option<&str>,
) -> Result<Workspace> {
    if session.is_empty() || session.len() > 200 || session.contains('\0') {
        return Err("Invalid session ID".into());
    }
    let cap = capability(cwd);
    if cap["supported"] != true {
        return Err(cap["reason"]
            .as_str()
            .unwrap_or("Unsupported filesystem")
            .into());
    }
    let source = repo(cwd)?;
    let project = repo(project.unwrap_or(&source))?;
    for entry in fs::read_dir(store).map_err(err)? {
        let entry = entry.map_err(err)?;
        if let Ok(w) = read_json::<Workspace>(&entry.path().join("workspace.json")) {
            if w.session_id == session {
                if Path::new(&w.source_cwd) != source || Path::new(&w.project_cwd) != project {
                    return Err("Session already owns another isolation workspace".into());
                }
                if identity(Path::new(&w.path))? != w.identity {
                    return Err("Session isolation root was replaced".into());
                }
                return Ok(w);
            }
        }
    }
    let source_head = text(&source, &["rev-parse", "HEAD"])?;
    let head = resolve_base(&source, base.unwrap_or("HEAD"))?;
    let token = session
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if token.is_empty() {
        return Err("Session ID must contain letters or numbers".into());
    }
    let branch = Some(format!("mc/{token}"));
    if text(
        &source,
        &[
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", branch.as_deref().unwrap_or_default()),
        ],
    )
    .is_ok()
    {
        return Err("The generated session branch already exists; start a new session".into());
    }
    let before = fingerprint(&source)?;
    let eligible = paths(&source)?;
    let excluded = before
        .keys()
        .filter(|p| !eligible.contains(*p))
        .cloned()
        .collect();
    let id = uuid::Uuid::new_v4().to_string();
    let parent = project.with_file_name(format!(
        "{}-cow",
        project
            .file_name()
            .ok_or("Invalid repository name")?
            .to_string_lossy()
    ));
    private_dir(&parent)?;
    let path = parent.join(&id);
    let record = store.join(&id);
    private_dir(&record)?;
    let result = (|| {
        clone_tree(&source, &path, true)?;
        private_git(
            &source,
            &path,
            &source_head,
            branch.as_deref().unwrap_or("cow"),
        )?;
        if before != fingerprint(&source)?
            || before != fingerprint(&path)?
            || text(&source, &["rev-parse", "HEAD"])? != source_head
            || paths(&source)? != eligible
        {
            return Err("Source changed while cloning; retry".into());
        }
        if head != source_head {
            text(
                &path,
                &[
                    "checkout",
                    "--quiet",
                    "--no-overwrite-ignore",
                    "-B",
                    branch.as_deref().ok_or("Missing session branch")?,
                    &head,
                ],
            )?;
        }
        let eligible = paths(&path)?;
        let base = snapshot_directory(store, &path, &id)?;
        init_repo(
            &base,
            &text(&source, &["rev-parse", "--show-object-format"])?,
        )?;
        let baseline = tree(&base, &path, &eligible, &excluded)?;
        let w = Workspace {
            id: id.clone(),
            path: path.to_string_lossy().into_owned(),
            source_cwd: source.to_string_lossy().into_owned(),
            project_cwd: project.to_string_lossy().into_owned(),
            session_id: session.into(),
            branch,
            head,
            dirty: None,
            unpushed: None,
            baseline,
            excluded,
            identity: identity(&path)?,
            git_config_version: 2,
            removal_path: None,
        };
        write_json(&record.join("workspace.json"), &w)?;
        Ok(w)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&path);
        if let Ok(snapshot) = snapshot_directory(store, &path, &id) {
            let _ = fs::remove_dir_all(snapshot);
        }
        let _ = fs::remove_dir_all(record);
    }
    result
}
fn current(store: &Path, w: &Workspace) -> Result<(PathBuf, String)> {
    let objects = snapshot_directory(store, Path::new(&w.path), &w.id)?;
    let root = Path::new(&w.path);
    let mut eligible = paths(root)?;
    let original = git(
        &objects,
        &["ls-tree", "-r", "--name-only", "-z", &w.baseline],
        None,
    )?;
    for p in original.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        eligible.insert(String::from_utf8(p.to_vec()).map_err(err)?);
    }
    let t = tree(&objects, root, &eligible, &w.excluded)?;
    Ok((objects, t))
}
fn changed(objects: &Path, base: &str, next: &str) -> Result<Vec<(String, String)>> {
    let b = git(
        objects,
        &[
            "diff",
            "--no-renames",
            "--name-status",
            "-z",
            base,
            next,
            "--",
        ],
        None,
    )?;
    let fields: Vec<_> = b.split(|b| *b == 0).filter(|b| !b.is_empty()).collect();
    let mut out = vec![];
    for pair in fields.chunks_exact(2) {
        out.push((
            String::from_utf8(pair[1].to_vec()).map_err(err)?,
            String::from_utf8(pair[0].to_vec()).map_err(err)?,
        ))
    }
    Ok(out)
}
fn status(store: &Path, w: &Workspace) -> Result<Value> {
    let (o, t) = current(store, w)?;
    let mut counts = BTreeMap::new();
    let stats = git(
        &o,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            &w.baseline,
            &t,
            "--",
        ],
        None,
    )?;
    for record in stats.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let fields = record.splitn(3, |b| *b == b'\t').collect::<Vec<_>>();
        if fields.len() == 3 {
            let number = |b: &[u8]| String::from_utf8_lossy(b).parse::<i64>().unwrap_or(0);
            counts.insert(
                String::from_utf8(fields[2].to_vec()).map_err(err)?,
                (number(fields[0]), number(fields[1])),
            );
        }
    }
    let files=changed(&o,&w.baseline,&t)?.into_iter().map(|(p,s)|{let (a,d)=counts.get(&p).copied().unwrap_or_default();json!({"path":Path::new(&w.path).join(&p),"relative":p,"status":s,"additions":a,"deletions":d,"staged":false,"unstaged":true})}).collect::<Vec<_>>();
    Ok(json!({"files":files}))
}
fn file_diff(store: &Path, w: &Workspace, p: &str) -> Result<Value> {
    relative(p)?;
    let (o, t) = current(store, w)?;
    let size = |revision: &str| {
        text(&o, &["cat-file", "-s", &format!("{revision}:{p}")])
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let large = size(&w.baseline) > 2 * 1024 * 1024 || size(&t) > 2 * 1024 * 1024;
    let read = |rev: &str| -> Result<Vec<u8>> {
        if large {
            Ok(vec![])
        } else {
            Ok(git(&o, &["show", &format!("{rev}:{p}")], None).unwrap_or_default())
        }
    };
    let a = read(&w.baseline)?;
    let b = read(&t)?;
    let binary = a.contains(&0)
        || b.contains(&0)
        || std::str::from_utf8(&a).is_err()
        || std::str::from_utf8(&b).is_err();
    let status = changed(&o, &w.baseline, &t)?
        .into_iter()
        .find(|(path, _)| path == p)
        .map(|(_, s)| s)
        .unwrap_or_else(|| "M".into());
    Ok(
        json!({"path":Path::new(&w.path).join(p),"relative":p,"status":status,"original":if binary||large{String::new()}else{String::from_utf8_lossy(&a).into_owned()},"current":if binary||large{String::new()}else{String::from_utf8_lossy(&b).into_owned()},"binary":binary,"tooLarge":large}),
    )
}
#[cfg(unix)]
fn destination_parent(root: &Path, p: &str) -> Result<(File, std::ffi::CString)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    relative(p)?;
    let parts = Path::new(p).components().collect::<Vec<_>>();
    let mut parent = open_dir(root)?;
    for component in &parts[..parts.len() - 1] {
        let name = std::ffi::CString::new(component.as_os_str().as_bytes()).map_err(err)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(err(std::io::Error::last_os_error()));
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
            )
        };
        if fd < 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        parent = unsafe { File::from_raw_fd(fd) };
    }
    Ok((
        parent,
        std::ffi::CString::new(parts.last().ok_or("Missing path")?.as_os_str().as_bytes())
            .map_err(err)?,
    ))
}
#[cfg(unix)]
fn apply(store: &Path, w: &Workspace, destination: &Path) -> Result<Value> {
    let destination = repo(destination)?;
    validate_repo(&destination)?;
    if destination != Path::new(&w.project_cwd) && destination != Path::new(&w.source_cwd) {
        return Err("Worker integration destination is outside its project".into());
    }
    let (objects, next) = current(store, w)?;
    let files = changed(&objects, &w.baseline, &next)?
        .into_iter()
        .map(|(p, _)| p)
        .collect::<Vec<_>>();
    let eligible = files.iter().cloned().collect::<BTreeSet<_>>();
    let target_tree = tree(&objects, &destination, &eligible, &BTreeSet::new())?;
    let entry = |revision: &str, p: &str| -> Result<Option<(String, String)>> {
        let line = text(&objects, &["ls-tree", revision, "--", p])?;
        let mut fields = line.split_whitespace();
        let mode = fields.next();
        fields.next();
        let oid = fields.next();
        Ok(mode.zip(oid).map(|(m, o)| (m.to_string(), o.to_string())))
    };
    let mut pending = Vec::new();
    let mut already = 0;
    for p in &files {
        let target = entry(&target_tree, p)?;
        if target == entry(&next, p)? {
            already += 1;
            continue;
        }
        if target != entry(&w.baseline, p)? {
            return Err(format!(
                "Cannot integrate {p}: lead checkout changed; worker was retained"
            ));
        }
        pending.push(p.clone());
    }
    // Read immutable captured blobs, so later worker edits cannot change this integration.
    #[cfg(unix)]
    for p in &pending {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let (parent, name) = destination_parent(&destination, p)?;
        match entry(&next, p)? {
            None => {
                if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                    return Err(err(std::io::Error::last_os_error()));
                }
            }
            Some((mode, oid)) => {
                let temporary =
                    std::ffi::CString::new(format!(".monocode-{}", uuid::Uuid::new_v4()))
                        .map_err(err)?;
                if mode == "120000" {
                    let raw = git(&objects, &["cat-file", "blob", &oid], None)?;
                    let link = PathBuf::from(std::ffi::OsStr::from_bytes(&raw));
                    let link = if link.is_absolute() {
                        destination.join(
                            link.strip_prefix(Path::new(&w.path))
                                .map_err(|_| "External integration symlink")?,
                        )
                    } else {
                        link
                    };
                    let link = std::ffi::CString::new(link.as_os_str().as_bytes()).map_err(err)?;
                    if unsafe {
                        libc::symlinkat(link.as_ptr(), parent.as_raw_fd(), temporary.as_ptr())
                    } != 0
                    {
                        return Err(err(std::io::Error::last_os_error()));
                    }
                } else {
                    let fd = unsafe {
                        libc::openat(
                            parent.as_raw_fd(),
                            temporary.as_ptr(),
                            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                            0o600,
                        )
                    };
                    if fd < 0 {
                        return Err(err(std::io::Error::last_os_error()));
                    }
                    let out = unsafe { File::from_raw_fd(fd) };
                    let output = git_command(&objects)
                        .args(["cat-file", "blob", &oid])
                        .stdout(Stdio::from(out.try_clone().map_err(err)?))
                        .output()
                        .map_err(err)?;
                    if !output.status.success() {
                        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
                    }
                    out.set_permissions(fs::Permissions::from_mode(if mode == "100755" {
                        0o755
                    } else {
                        0o644
                    }))
                    .map_err(err)?;
                    out.sync_all().map_err(err)?;
                }
                if unsafe {
                    libc::renameat(
                        parent.as_raw_fd(),
                        temporary.as_ptr(),
                        parent.as_raw_fd(),
                        name.as_ptr(),
                    )
                } != 0
                {
                    return Err(err(std::io::Error::last_os_error()));
                }
            }
        }
    }
    Ok(json!({"files":files,"alreadyApplied":already}))
}
#[cfg(not(unix))]
fn apply(_: &Path, _: &Workspace, _: &Path) -> Result<Value> {
    Err(UNSUPPORTED_FILESYSTEM.into())
}
fn unpreserved_commits(w: &Workspace) -> Result<u64> {
    let root = Path::new(&w.path);
    let heads = text(
        root,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )?;
    let mut retained = BTreeSet::new();
    for oid in heads.lines() {
        let Ok(head) = text(
            root,
            &["rev-parse", "--verify", &format!("{oid}^{{commit}}")],
        ) else {
            continue;
        };
        for source in [Path::new(&w.project_cwd), Path::new(&w.source_cwd)] {
            if source != root
                && text(
                    source,
                    &[
                        "for-each-ref",
                        "--format=%(refname)",
                        "--contains",
                        &head,
                        "refs/heads",
                        "refs/tags",
                        "refs/remotes",
                    ],
                )
                .is_ok_and(|refs| !refs.is_empty())
            {
                retained.insert(head.clone());
                break;
            }
        }
    }
    let mut arguments = vec![
        "rev-list",
        "--count",
        "--branches",
        "--tags",
        "--not",
        "--remotes",
    ];
    arguments.extend(retained.iter().map(String::as_str));
    text(root, &arguments)?.parse().map_err(err)
}
fn git_dirty(root: &Path) -> Result<bool> {
    let dirty = !git(
        root,
        &["status", "--porcelain", "--untracked-files=all"],
        None,
    )?
    .is_empty();
    if dirty && text(root, &["config", "--get-regexp", "^filter\\."]).is_ok() {
        return Err("Cannot verify cleanliness without running Git filters. Review Git Changes and confirm deletion to remove this copy; commits and stashes will be kept.".into());
    }
    Ok(dirty)
}
fn check_remove(w: &Workspace, force: bool) -> Result<Value> {
    if identity(Path::new(&w.path))? != w.identity {
        return Err("Isolation root was replaced".into());
    }
    let source = repo(Path::new(&w.project_cwd))?;
    if source == Path::new(&w.path) {
        return Err("Cannot preserve history in the isolation being removed".into());
    }
    for root in [&source, &PathBuf::from(&w.path)] {
        let common = PathBuf::from(text(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        identity(&common)?;
        identity(&common.join("objects"))?;
        if common.join("objects/info/alternates").exists() {
            return Err("Git object alternates are unsupported".into());
        }
        if root == Path::new(&w.path) && common != root.join(".git") {
            return Err("Isolation Git directory was replaced".into());
        }
    }
    if !force && git_dirty(Path::new(&w.path))? {
        return Err("Copy-on-write has uncommitted changes; explicit force is required".into());
    }
    Ok(Value::Null)
}
fn preserve_history(w: &Workspace) -> Result<()> {
    let root = Path::new(&w.path);
    let source = repo(Path::new(&w.project_cwd))?;
    let mut refs = text(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )?;
    if text(root, &["symbolic-ref", "HEAD"]).is_err() {
        let head = text(root, &["rev-parse", "--verify", "HEAD"])?;
        refs.push_str(&format!("\nHEAD {head}"));
    }
    for line in refs.lines().filter(|line| !line.is_empty()) {
        let (reference, oid) = line.split_once(' ').ok_or("Invalid Git reference")?;
        let existing = text(&source, &["rev-parse", "--verify", reference]).ok();
        if existing.as_deref() == Some(oid) {
            continue;
        }
        if reference.starts_with("refs/heads/")
            && existing.as_ref().is_some_and(|head| {
                git(&source, &["merge-base", "--is-ancestor", oid, head], None).is_ok()
            })
        {
            continue;
        }
        let destination = if reference == "HEAD" {
            format!("refs/heads/mc/kept-{}/detached", w.id)
        } else if existing.is_some() {
            let (namespace, name) = if let Some(name) = reference.strip_prefix("refs/heads/") {
                ("refs/heads", name)
            } else {
                (
                    "refs/tags",
                    reference.strip_prefix("refs/tags/").ok_or("Invalid tag")?,
                )
            };
            format!("{namespace}/mc/kept-{}/{name}", w.id)
        } else {
            reference.to_string()
        };
        let retained = text(&source, &["rev-parse", "--verify", &destination]).ok();
        if retained.as_deref() == Some(oid) {
            continue;
        }
        let destination = if retained.is_some() {
            format!("{destination}-{oid}")
        } else {
            destination
        };
        // Import objects without changing HEAD, checked-out files, or existing refs.
        // An expected-empty update protects against concurrent ref creation.
        import_objects(&source, root, reference)?;
        text(&source, &["update-ref", &destination, oid, ""])?;
    }
    let mut retained = stash_entries(&source)?
        .into_iter()
        .map(|(oid, _)| oid)
        .collect::<BTreeSet<_>>();
    // Older stashes are reflog entries, not ancestors of the current stash tip.
    // Import oldest first so the stack remains usable after cleanup or a retry.
    for (oid, message) in stash_entries(root)?.into_iter().rev() {
        if retained.insert(oid.clone()) {
            import_objects(&source, root, &oid)?;
            text(&source, &["stash", "store", "-m", &message, &oid])?;
        }
    }
    Ok(())
}
fn stash_entries(root: &Path) -> Result<Vec<(String, String)>> {
    let found = git_command(root)
        .args(["show-ref", "--verify", "--quiet", "refs/stash"])
        .output()
        .map_err(err)?;
    if !found.status.success() {
        if found.status.code() != Some(1) {
            return Err(
                "Cannot read stash reference; history must be retained before deletion".into(),
            );
        }
        let log_path = PathBuf::from(text(
            root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "logs/refs/stash",
            ],
        )?);
        match fs::symlink_metadata(log_path) {
            Ok(_) => return Err(
                "Stash reflog exists without its reference; recover it before deleting this copy"
                    .into(),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(err(error)),
        }
    }
    let tip = text(root, &["rev-parse", "--verify", "refs/stash"])?;
    let log = text(
        root,
        &["reflog", "show", "--format=%H%x00%gs", "refs/stash"],
    )?;
    let mut entries = log
        .lines()
        .map(|line| {
            line.split_once('\0')
                .map(|(oid, message)| (oid.to_string(), message.to_string()))
                .ok_or_else(|| "Invalid stash reflog".to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    if !entries.iter().any(|(oid, _)| oid == &tip) {
        entries.insert(0, (tip, "Retained copy-on-write stash".into()));
    }
    Ok(entries)
}
fn import_objects(destination: &Path, source: &Path, reference: &str) -> Result<()> {
    text(
        destination,
        &[
            "-c",
            "uploadpack.packObjectsHook=git pack-objects",
            "fetch",
            "--no-recurse-submodules",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            source.to_str().ok_or("Non-UTF-8 Git path")?,
            reference,
        ],
    )?;
    Ok(())
}
fn cleanup_removed(store: &Path, w: &Workspace) -> Result<()> {
    id_valid(&w.id)?;
    match fs::symlink_metadata(&w.path) {
        Ok(_) => return Ok(()), // A crash before rename left the checkout intact.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(err(error)),
    }
    let tombstone = Path::new(w.removal_path.as_deref().ok_or("Missing removal path")?);
    if tombstone.parent() != Path::new(&w.path).parent()
        || !tombstone
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("remove-"))
    {
        return Err("Invalid removal path".into());
    }
    match fs::symlink_metadata(tombstone) {
        Ok(_) => {
            if identity(tombstone)? != w.identity {
                return Err("Isolation root changed during deletion".into());
            }
            fs::remove_dir_all(tombstone).map_err(err)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(err(error)),
    }
    let snapshots = [
        Path::new(&w.path)
            .parent()
            .ok_or("Invalid isolation root")?
            .join(".baselines")
            .join(&w.id),
        store.join(&w.id).join("baseline"),
    ];
    for snapshot in snapshots {
        match fs::symlink_metadata(&snapshot) {
            Ok(_) => {
                // Reject replaced parents/links rather than following them.
                identity(snapshot.parent().ok_or("Invalid baseline parent")?)?;
                identity(&snapshot)?;
                fs::remove_dir_all(snapshot).map_err(err)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(err(error)),
        }
    }
    fs::remove_dir_all(store.join(&w.id)).map_err(err)
}
fn remove(store: &Path, w: &Workspace, force: bool) -> Result<Value> {
    check_remove(w, force)?;
    preserve_history(w)?;
    let path = Path::new(&w.path);
    let tombstone = path.with_file_name(format!("remove-{}", uuid::Uuid::new_v4()));
    let mut removed = w.clone();
    removed.removal_path = Some(tombstone.to_string_lossy().into_owned());
    write_json(&store.join(&w.id).join("workspace.json"), &removed)?;
    fs::rename(path, &tombstone).map_err(err)?;
    if identity(&tombstone)? != w.identity {
        let _ = fs::rename(&tombstone, path);
        return Err("Isolation root changed during deletion".into());
    }
    // The owned checkout is gone. Metadata/leftover directory errors must not
    // cause the session-removal journal to restore sessions onto a missing path.
    // Keep the record so the next native request retries garbage collection.
    if let Err(error) = cleanup_removed(store, &removed) {
        eprintln!("Copy removed; isolation cleanup will retry: {error}");
    }
    Ok(Value::Null)
}
pub fn dispatch(store: &Path, request: Value) -> Result<Value> {
    private_dir(store)?;
    // ponytail: serialize registry operations; per-workspace locks if large captures contend.
    #[cfg(unix)]
    let _lock = {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(store.join(".lock"))
            .map_err(err)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        lock
    };
    let command = request["command"]
        .as_str()
        .ok_or("Missing isolation command")?;
    let a = &request["args"];
    let string = |key: &str| a[key].as_str().ok_or_else(|| format!("Missing {key}"));
    let cwd = Path::new(string("cwd")?);
    if !cfg!(target_os = "macos") {
        return match command {
            "cow_capability" => Ok(json!({"supported":false,"reason":UNSUPPORTED_FILESYSTEM})),
            "cow_list" => Ok(json!([])),
            _ => Err(UNSUPPORTED_FILESYSTEM.into()),
        };
    }
    for entry in fs::read_dir(store).map_err(err)? {
        let entry = entry.map_err(err)?;
        if let Ok(w) = read_json::<Workspace>(&entry.path().join("workspace.json")) {
            if w.removal_path.is_some() {
                if let Err(error) = cleanup_removed(store, &w) {
                    eprintln!("Isolation cleanup will retry: {error}");
                }
            }
        }
    }
    if command == "cow_capability" {
        return Ok(capability(cwd));
    }
    if command == "cow_create" {
        return serde_json::to_value(create(
            store,
            cwd,
            string("sessionId")?,
            a["projectCwd"].as_str().map(Path::new),
            a["base"].as_str(),
        )?)
        .map_err(err);
    }
    if command == "cow_list" {
        let cwd = canonical(cwd)?;
        let mut registered = vec![];
        for entry in fs::read_dir(store).map_err(err)? {
            let entry = entry.map_err(err)?;
            if let Ok(w) = read_json::<Workspace>(&entry.path().join("workspace.json")) {
                if identity(Path::new(&w.path)).is_ok_and(|i| i == w.identity) {
                    registered.push(w);
                }
            }
        }
        let project = registered
            .iter()
            .find(|w| Path::new(&w.path) == cwd || Path::new(&w.source_cwd) == cwd)
            .map(|w| PathBuf::from(&w.project_cwd))
            .unwrap_or(cwd);
        return serde_json::to_value(
            registered
                .into_iter()
                .filter(|w| Path::new(&w.project_cwd) == project)
                .filter_map(|mut w| {
                    if let Err(error) = upgrade_git_configuration(store, &mut w) {
                        eprintln!("Copy unavailable; its files and ownership record were retained ({}): {error}", w.id);
                        None
                    } else {
                        Some(w)
                    }
                })
                .map(|mut w| {
                    w.branch = text(Path::new(&w.path), &["symbolic-ref", "--short", "HEAD"]).ok();
                    w.head = text(Path::new(&w.path), &["rev-parse", "HEAD"]).unwrap_or_default();
                    w.dirty = git_dirty(Path::new(&w.path)).ok();
                    w.unpushed = unpreserved_commits(&w).ok();
                    let mut value = serde_json::to_value(&w).map_err(err)?;
                    value["rootIdentity"] =
                        json!([w.identity.0.to_string(), w.identity.1.to_string()]);
                    Ok(value)
                })
                .collect::<Result<Vec<_>>>()?,
        )
        .map_err(err);
    }
    let w = load(store, cwd, string("cowId")?)?;
    match command {
        "cow_apply" => apply(store, &w, Path::new(string("toCwd")?)),
        "cow_status" => status(store, &w),
        "cow_file_diff" => file_diff(store, &w, string("relative")?),
        "cow_check_remove" => check_remove(&w, a["force"].as_bool().unwrap_or(false)),
        "cow_remove" => remove(store, &w, a["force"].as_bool().unwrap_or(false)),
        _ => Err("Unknown isolation command".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("monocode-cow-test-{}", uuid::Uuid::new_v4()));
            private_dir(&p).unwrap();
            Self(p)
        }
        fn repo(&self) -> PathBuf {
            let p = self.0.join("project");
            init_repo(&p, "sha1").unwrap();
            text(&p, &["config", "user.name", "Test"]).unwrap();
            text(&p, &["config", "user.email", "test@example.invalid"]).unwrap();
            fs::write(p.join("app.txt"), "first\nsecond\nthird\n").unwrap();
            fs::write(p.join(".gitignore"), "deps/\n").unwrap();
            text(&p, &["add", "."]).unwrap();
            text(&p, &["commit", "-m", "initial"]).unwrap();
            p
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn ordinary_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
    fn native_supported(source: &Path) -> bool {
        let cap = capability(source);
        if cap["supported"] == true {
            true
        } else {
            assert!(std::env::var_os("MONOCODE_REQUIRE_COW").is_none(), "{cap}");
            eprintln!("SKIP native APFS coverage: {cap}");
            false
        }
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_refuses_cow_without_affecting_local_repository() {
        let t = Temp::new();
        let source = t.repo();
        let store = t.0.join("registry");
        let probe = dispatch(
            &store,
            json!({"command":"cow_capability","args":{"cwd":source}}),
        )
        .unwrap();
        assert_eq!(probe["supported"], false);
        assert_eq!(probe["reason"], UNSUPPORTED_FILESYSTEM);
        assert_eq!(
            dispatch(&store, json!({"command":"cow_list","args":{"cwd":source}})).unwrap(),
            json!([])
        );
        assert!(dispatch(
            &store,
            json!({"command":"cow_create","args":{"cwd":source,"sessionId":"unsupported"}})
        )
        .unwrap_err()
        .contains("APFS on macOS"));
        assert!(!source.with_file_name("project-cow").exists());
        assert_eq!(text(&source, &["status", "--porcelain"]).unwrap(), "");
    }
    #[test]
    fn failed_copy_upgrade_does_not_block_healthy_workspaces_or_other_projects() {
        let t = Temp::new();
        let source = t.repo();
        if !native_supported(&source) {
            return;
        }
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        let donor = create(&store, &source, "donor-session", None, None).unwrap();
        let mut legacy = create(
            &store,
            Path::new(&donor.path),
            "legacy-session",
            Some(&source),
            None,
        )
        .unwrap();
        remove(&store, &donor, false).unwrap();
        legacy.git_config_version = 1;
        write_json(&store.join(&legacy.id).join("workspace.json"), &legacy).unwrap();
        let healthy = create(&store, &source, "healthy-session", None, None).unwrap();
        let listed = dispatch(&store, json!({"command":"cow_list","args":{"cwd":source}})).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["id"], healthy.id);
        assert!(Path::new(&legacy.path).exists());
        assert!(store.join(&legacy.id).join("workspace.json").exists());
        assert!(load(&store, &source, &legacy.id).is_err());
        let ordinary_project = Temp::new();
        let ordinary_root = ordinary_project.repo();
        assert_eq!(
            dispatch(
                &store,
                json!({"command":"cow_list","args":{"cwd":ordinary_root}})
            )
            .unwrap(),
            json!([])
        );
        assert_eq!(
            text(&ordinary_root, &["status", "--porcelain"]).unwrap(),
            ""
        );
    }
    #[test]
    fn cleanup_keeps_every_stash_and_retries_after_irreversible_removal() {
        let t = Temp::new();
        let source = t.repo();
        if !native_supported(&source) {
            return;
        }
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        fs::write(source.join("app.txt"), "source stash\n").unwrap();
        let source_stash = ordinary_git(&source, &["stash", "push", "-m", "source stash"]);
        assert!(!source_stash.is_empty());
        let original_stash = text(&source, &["rev-parse", "refs/stash"]).unwrap();
        let w = create(&store, &source, "stash-session", None, None).unwrap();
        let root = Path::new(&w.path);
        let mut stash_ids = vec![];
        for message in ["older copy stash", "newer copy stash"] {
            fs::write(root.join("app.txt"), format!("{message}\n")).unwrap();
            ordinary_git(root, &["stash", "push", "-m", message]);
            stash_ids.push(text(root, &["rev-parse", "refs/stash"]).unwrap());
        }
        let stash_ref = root.join(".git/refs/stash");
        let stash_bytes = fs::read(&stash_ref).unwrap();
        fs::remove_file(&stash_ref).unwrap();
        assert!(stash_entries(root).unwrap_err().contains("Stash reflog"));
        fs::write(&stash_ref, "corrupt reference\n").unwrap();
        assert!(remove(&store, &w, true).is_err());
        assert!(
            root.exists(),
            "Corrupt stash metadata must block even forced deletion"
        );
        fs::write(&stash_ref, stash_bytes).unwrap();
        assert!(!git_dirty(root).unwrap());
        preserve_history(&w).unwrap();
        preserve_history(&w).unwrap(); // Retry must not duplicate the stash stack.
        let retained = stash_entries(&source).unwrap();
        assert_eq!(retained.len(), 3);
        assert_eq!(
            retained
                .iter()
                .map(|(oid, _)| oid.clone())
                .collect::<Vec<_>>(),
            vec![stash_ids[1].clone(), stash_ids[0].clone(), original_stash]
        );
        let source_before = fs::read(source.join("app.txt")).unwrap();
        #[cfg(unix)]
        {
            let baseline = snapshot_directory(&store, root, &w.id).unwrap();
            let saved = baseline.with_file_name(format!("saved-{}", w.id));
            fs::rename(&baseline, &saved).unwrap();
            std::os::unix::fs::symlink(&source, &baseline).unwrap();
            remove(&store, &w, false).unwrap();
            assert!(!root.exists());
            assert!(store.join(&w.id).join("workspace.json").exists());
            assert_eq!(fs::read(source.join("app.txt")).unwrap(), source_before);
            fs::remove_file(&baseline).unwrap();
            fs::rename(saved, &baseline).unwrap();
            dispatch(&store, json!({"command":"cow_list","args":{"cwd":source}})).unwrap();
            assert!(!baseline.exists());
            assert!(!store.join(&w.id).exists());
        }
        for (index, oid) in stash_ids.iter().enumerate() {
            assert_eq!(
                text(&source, &["show", &format!("{oid}:app.txt")]).unwrap(),
                if index == 0 {
                    "older copy stash"
                } else {
                    "newer copy stash"
                }
            );
        }
    }
    #[test]
    fn normal_git_keeps_conversion_settings_and_existing_copies_are_upgraded() {
        let t = Temp::new();
        let source = t.repo();
        if !native_supported(&source) {
            return;
        }
        let marker = t.0.join("filter-ran");
        let clean = format!("touch {}; sed s/WORLD/PTR/", marker.display());
        let smudge = format!("touch {}; sed s/PTR/WORLD/", marker.display());
        for (key, value) in [
            ("core.autocrlf", "true"),
            ("core.fileMode", "false"),
            ("core.eol", "crlf"),
            ("filter.convert.clean", clean.as_str()),
            ("filter.convert.smudge", smudge.as_str()),
            ("filter.convert.required", "true"),
            ("gpg.program", "/custom/signing-program"),
        ] {
            text(&source, &["config", key, value]).unwrap();
        }
        fs::write(source.join(".gitattributes"), "app.txt filter=convert\n").unwrap();
        private_dir(&source.join(".git/info")).unwrap();
        fs::write(source.join(".git/info/exclude"), "private.env\n").unwrap();
        fs::write(source.join("ignore-rules"), "outside.env\n").unwrap();
        fs::write(source.join("attribute-rules"), "app.txt text\n").unwrap();
        text(&source, &["config", "core.excludesFile", "ignore-rules"]).unwrap();
        text(
            &source,
            &["config", "core.attributesFile", "attribute-rules"],
        )
        .unwrap();
        fs::write(source.join("private.env"), "PRIVATE_SECRET\n").unwrap();
        fs::write(source.join("outside.env"), "OUTSIDE_SECRET\n").unwrap();
        fs::write(source.join("app.txt"), "WORLD\r\n").unwrap();
        ordinary_git(&source, &["add", "."]);
        ordinary_git(&source, &["commit", "-m", "filtered file"]);
        assert_eq!(ordinary_git(&source, &["status", "--porcelain"]), "");
        fs::remove_file(&marker).unwrap();
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        let mut w = create(&store, &source, "conversion-session", None, None).unwrap();
        let root = Path::new(&w.path);
        status(&store, &w).unwrap();
        dispatch(&store, json!({"command":"cow_list","args":{"cwd":source}})).unwrap();
        assert!(
            !marker.exists(),
            "Native operations must not run the filter"
        );
        assert_eq!(ordinary_git(root, &["status", "--porcelain"]), "");
        ordinary_git(root, &["add", "."]);
        assert_eq!(ordinary_git(root, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(
            ordinary_git(root, &["check-ignore", "private.env", "outside.env"]),
            "private.env\noutside.env"
        );
        assert_eq!(
            ordinary_git(root, &["config", "--get", "core.excludesFile"]),
            root.join("ignore-rules").to_string_lossy()
        );
        assert_eq!(
            ordinary_git(root, &["config", "--get", "core.attributesFile"]),
            root.join("attribute-rules").to_string_lossy()
        );
        fs::remove_file(&marker).unwrap();
        if let Err(error) = check_remove(&w, false) {
            assert!(error.contains("without running Git filters"));
        }
        assert!(!marker.exists());
        fs::write(root.join("app.txt"), "WORLD changed\r\n").unwrap();
        assert!(check_remove(&w, false).is_err());
        assert!(!marker.exists());
        ordinary_git(root, &["add", "app.txt"]);
        assert_eq!(text(root, &["show", ":app.txt"]).unwrap(), "PTR changed");
        ordinary_git(root, &["commit", "-m", "normal filtered commit"]);
        assert_eq!(ordinary_git(root, &["status", "--porcelain"]), "");
        for key in [
            "core.autocrlf",
            "core.eol",
            "filter.convert.clean",
            "filter.convert.smudge",
            "filter.convert.required",
            "gpg.program",
            "core.excludesFile",
            "core.attributesFile",
        ] {
            text(root, &["config", "--unset-all", key]).unwrap();
        }
        text(root, &["config", "core.fileMode", "true"]).unwrap();
        fs::remove_file(root.join(".git/info/exclude")).unwrap();
        w.git_config_version = 1;
        write_json(&store.join(&w.id).join("workspace.json"), &w).unwrap();
        let upgraded = load(&store, &source, &w.id).unwrap();
        assert_eq!(upgraded.git_config_version, 2);
        assert_eq!(
            ordinary_git(root, &["check-ignore", "private.env", "outside.env"]),
            "private.env\noutside.env"
        );
        assert_eq!(
            text(root, &["config", "--get", "core.autocrlf"]).unwrap(),
            "true"
        );
        assert_eq!(
            config_values(root, "filter.convert.clean", true).unwrap(),
            vec![clean]
        );
        assert_eq!(
            text(root, &["config", "--get", "gpg.program"]).unwrap(),
            "/custom/signing-program"
        );
        assert_eq!(
            text(root, &["config", "--get", "core.fileMode"]).unwrap(),
            "true",
            "Keep explicit configuration chosen in the copy"
        );
        fs::remove_file(&marker).unwrap();
        remove(&store, &upgraded, true).unwrap();
        assert!(!marker.exists(), "Cleanup must not run filters");
    }
    #[test]
    fn trust_boundary_paths_and_git_routing() {
        for p in ["", "../escape", "/tmp/escape", "a/../escape", "a\0b"] {
            assert!(relative(p).is_err(), "{p:?}");
        }
        assert!(relative("normal/file.txt").is_ok());
        assert!(id_valid("../escape").is_err());
        let t = Temp::new();
        let repo = t.repo();
        let cmd = git_command(&repo);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "GIT_CONFIG_GLOBAL"
                    && v == Some(std::ffi::OsStr::new("/dev/null")))
        );
        assert!(cmd.get_args().any(|s| s == "core.hooksPath=/dev/null"));
        text(&repo, &["update-index", "--assume-unchanged", "app.txt"]).unwrap();
        assert!(validate_repo(&repo)
            .unwrap_err()
            .contains("assume-unchanged"));
        text(&repo, &["update-index", "--no-assume-unchanged", "app.txt"]).unwrap();
        text(&repo, &["update-index", "--skip-worktree", "app.txt"]).unwrap();
        assert!(validate_repo(&repo).unwrap_err().contains("Skip-worktree"));
    }
    #[test]
    fn native_clone_delta_worker_and_cleanup() {
        let t = Temp::new();
        let source = t.repo();
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        if capability(&source)["supported"] != true {
            assert!(
                std::env::var_os("MONOCODE_REQUIRE_COW").is_none(),
                "Native CoW coverage is required: {}",
                capability(&source)
            );
            eprintln!("SKIP native APFS coverage: {}", capability(&source));
            return;
        }
        fs::write(source.join("app.txt"), "first\ninherited\nthird\n").unwrap();
        private_dir(&source.join("deps")).unwrap();
        fs::write(source.join("deps/pkg"), "dependency").unwrap();
        let w = create(&store, &source, "session", None, None).unwrap();
        let clone = Path::new(&w.path);
        let snapshot = snapshot_directory(&store, clone, &w.id).unwrap();
        assert!(snapshot.starts_with(clone.parent().unwrap().join(".baselines")));
        assert!(!snapshot.starts_with(&store));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&snapshot).unwrap().dev(),
                fs::metadata(clone).unwrap().dev()
            );
        }
        assert_eq!(fs::read(clone.join("deps/pkg")).unwrap(), b"dependency");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                fs::metadata(source.join("app.txt")).unwrap().ino(),
                fs::metadata(clone.join("app.txt")).unwrap().ino()
            );
        }
        fs::write(clone.join("new.bin"), [0, 1, 2, 255]).unwrap();
        fs::write(clone.join("deps/pkg"), "mutated ignored dependency").unwrap();
        fs::write(clone.join("app.txt"), "first\ninherited\nthird\nagent\n").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("app.txt")).unwrap(),
            "first\ninherited\nthird\n"
        );
        let snapshot = status(&store, &w).unwrap();
        let files = snapshot["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f["relative"] != "deps/pkg"));
        let result = apply(&store, &w, &source).unwrap();
        assert_eq!(result["alreadyApplied"], 0);
        assert_eq!(apply(&store, &w, &source).unwrap()["alreadyApplied"], 2);
        fs::write(clone.join("later.txt"), "later").unwrap();
        assert!(remove(&store, &w, false).is_err());
        text(clone, &["add", "app.txt", "new.bin", "later.txt"]).unwrap();
        text(clone, &["commit", "-m", "session"]).unwrap();
        let head = text(clone, &["rev-parse", "HEAD"]).unwrap();
        remove(&store, &w, false).unwrap();
        assert_eq!(
            text(&source, &["rev-parse", w.branch.as_deref().unwrap()]).unwrap(),
            head
        );
        assert!(!clone.exists());
    }
    #[cfg(unix)]
    #[test]
    fn linked_worktree_private_git_and_external_symlink_rejection() {
        let t = Temp::new();
        let source = t.repo();
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        if capability(&source)["supported"] != true {
            assert!(
                std::env::var_os("MONOCODE_REQUIRE_COW").is_none(),
                "Native CoW coverage is required: {}",
                capability(&source)
            );
            eprintln!("SKIP native APFS coverage: {}", capability(&source));
            return;
        }
        let linked = t.0.join("linked");
        text(
            &source,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        )
        .unwrap();
        std::os::unix::fs::symlink(linked.join("app.txt"), linked.join("internal-link")).unwrap();
        let w = create(&store, &linked, "linked-session", None, None).unwrap();
        let clone = Path::new(&w.path);
        assert!(clone.join(".git").is_dir());
        assert!(!fs::read_link(clone.join("internal-link"))
            .unwrap()
            .is_absolute());
        assert_eq!(
            fs::read_to_string(clone.join("internal-link")).unwrap(),
            "first\nsecond\nthird\n"
        );
        assert_eq!(
            text(clone, &["rev-parse", "--git-common-dir"]).unwrap(),
            ".git"
        );
        let snapshot = snapshot_directory(&store, clone, &w.id).unwrap();
        let legacy = store.join(&w.id).join("baseline");
        fs::rename(snapshot, &legacy).unwrap();
        assert_eq!(snapshot_directory(&store, clone, &w.id).unwrap(), legacy);
        assert_eq!(status(&store, &w).unwrap()["files"], json!([]));
        fs::write(t.0.join("outside"), "secret").unwrap();
        std::os::unix::fs::symlink(t.0.join("outside"), source.join("escaping-link")).unwrap();
        assert!(create(&store, &source, "unsafe", None, None)
            .unwrap_err()
            .contains("External symlink"));
    }
    #[test]
    fn complete_large_delta_and_frozen_ignored_files() {
        let t = Temp::new();
        let source = t.repo();
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        if capability(&source)["supported"] != true {
            assert!(
                std::env::var_os("MONOCODE_REQUIRE_COW").is_none(),
                "Native CoW coverage required"
            );
            eprintln!("SKIP native APFS");
            return;
        }
        private_dir(&source.join("deps")).unwrap();
        fs::write(source.join("deps/secret"), "inherited ignored").unwrap();
        let w = create(&store, &source, "wide", None, None).unwrap();
        let root = Path::new(&w.path);
        for n in 0..501 {
            fs::write(root.join(format!("file-{n}.txt")), format!("{n}\n")).unwrap();
        }
        fs::write(root.join("large.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        fs::write(root.join(".gitignore"), "").unwrap();
        text(root, &["add", "-f", "deps/secret"]).unwrap();
        let result = status(&store, &w).unwrap();
        assert_eq!(result["files"].as_array().unwrap().len(), 503);
        assert!(result["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["relative"] != "deps/secret"));
        assert_eq!(
            file_diff(&store, &w, "large.bin").unwrap()["tooLarge"],
            true
        );
        let listed = dispatch(&store, json!({"command":"cow_list","args":{"cwd":w.path}})).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert!(load(&store, &t.0, &w.id).is_err());
    }
    #[test]
    fn internal_clone_review_and_cleanup_do_not_execute_filters_or_hooks() {
        let t = Temp::new();
        let source = t.repo();
        let store = t.0.join("registry");
        private_dir(&store).unwrap();
        if capability(&source)["supported"] != true {
            assert!(
                std::env::var_os("MONOCODE_REQUIRE_COW").is_none(),
                "Native CoW coverage required"
            );
            eprintln!("SKIP native APFS");
            return;
        }
        fs::write(source.join(".gitattributes"), "app.txt filter=hostile\n").unwrap();
        text(&source, &["add", "."]).unwrap();
        text(&source, &["commit", "-m", "attributes"]).unwrap();
        let marker = t.0.join("executed");
        let executable = format!("touch {}; cat", marker.display());
        let included = t.0.join("included.gitconfig");
        fs::write(&included, format!("[filter \"hostile\"]\nclean = \"{executable}\"\nsmudge = \"{executable}\"\nrequired = true\n")).unwrap();
        text(
            &source,
            &["config", "include.path", included.to_str().unwrap()],
        )
        .unwrap();
        let w = create(&store, &source, "filters", None, None).unwrap();
        fs::write(Path::new(&w.path).join("new.txt"), "session").unwrap();
        assert_eq!(
            config_values(Path::new(&w.path), "filter.hostile.clean", true).unwrap(),
            vec![executable]
        );
        status(&store, &w).unwrap();
        dispatch(&store, json!({"command":"cow_list","args":{"cwd":source}})).unwrap();
        remove(&store, &w, true).unwrap();
        assert!(!marker.exists());
    }
}
