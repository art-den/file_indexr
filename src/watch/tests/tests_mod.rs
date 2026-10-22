use super::*;

use crate::testutil::unique_temp_dir;
use notify::event::{
    AccessKind, CreateKind, Flag, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};

/// Non-existent index dir for tests that use fixed `/tmp` paths — none of
/// the fixture paths below are skipped with this context.
const TMP_INDEX: &str = "/tmp/file_indexr_unused_index";

/// Comparable projection of `FileChange` so tests can assert the exact
/// emitted sequence (variant, path, order) with `assert_eq!`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Chg {
    Modified(PathBuf),
    Deleted(PathBuf),
    DeletedDir(PathBuf),
    Rescan,
}

impl From<&FileChange> for Chg {
    fn from(c: &FileChange) -> Self {
        match c {
            FileChange::Modified(p) => Chg::Modified(p.clone()),
            FileChange::Deleted(p) => Chg::Deleted(p.clone()),
            FileChange::DeletedDir(p) => Chg::DeletedDir(p.clone()),
            FileChange::Rescan => Chg::Rescan,
        }
    }
}

fn chgs(changes: &[FileChange]) -> Vec<Chg> {
    changes.iter().map(Chg::from).collect()
}

/// Run `event_to_changes` on a synthesized event.
async fn event_changes(
    kind: EventKind,
    paths: Vec<&Path>,
    watched: &Path,
    index: &Path,
) -> Vec<FileChange> {
    let event = Event {
        kind,
        paths: paths.into_iter().map(PathBuf::from).collect(),
        attrs: Default::default(),
    };
    event_to_changes(&event, watched, index).await
}

/// `event_changes` for rename (ModifyKind::Name) events.
async fn rename_changes(
    mode: RenameMode,
    paths: Vec<&Path>,
    watched: &Path,
    index: &Path,
) -> Vec<FileChange> {
    event_changes(
        EventKind::Modify(ModifyKind::Name(mode)),
        paths,
        watched,
        index,
    )
    .await
}

/// Remove-kind event under the fixed `/tmp` watched context.
async fn remove_changes(kind: RemoveKind, path: &Path) -> Vec<Chg> {
    let changes = event_changes(
        EventKind::Remove(kind),
        vec![path],
        Path::new("/tmp"),
        Path::new(TMP_INDEX),
    )
    .await;
    chgs(&changes)
}

#[tokio::test]
async fn test_create_file_emits_modified() {
    let dir = unique_temp_dir("watch_create_file");
    let file_path = dir.join("new.txt");
    std::fs::write(&file_path, "hello").unwrap();

    let changes = event_changes(
        EventKind::Create(CreateKind::File),
        vec![&file_path],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert_eq!(chgs(&changes), [Chg::Modified(file_path)]);
}

#[tokio::test]
async fn test_modify_emits_modified() {
    let dir = unique_temp_dir("watch_modify");
    let file_path = dir.join("modified.txt");
    std::fs::write(&file_path, "hello").unwrap();

    let changes = event_changes(
        EventKind::Modify(ModifyKind::Any),
        vec![&file_path],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert_eq!(chgs(&changes), [Chg::Modified(file_path)]);
}

#[tokio::test]
async fn test_modify_dir_emits_nothing() {
    let dir = unique_temp_dir("watch_modify_dir");
    // A file inside the dir must NOT be picked up by a Modify on the dir.
    std::fs::write(dir.join("inner.txt"), "hello").unwrap();

    let changes = event_changes(
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        vec![&dir],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_modify_nonexistent_emits_nothing() {
    let dir = unique_temp_dir("watch_modify_missing");
    let path = dir.join("missing.txt"); // Never created.

    let changes = event_changes(
        EventKind::Modify(ModifyKind::Any),
        vec![&path],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_create_dir_emits_modified_for_files() {
    let dir = unique_temp_dir("watch_create_dir_nonempty");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    std::fs::write(dir.join("b.txt"), "world").unwrap();

    let changes = event_changes(
        EventKind::Create(CreateKind::Folder),
        vec![&dir],
        &dir,
        &dir.join("index"),
    )
    .await;
    // Create on a directory still walks it and emits Modified per file.
    let mut got = chgs(&changes);
    got.sort();
    assert_eq!(
        got,
        [
            Chg::Modified(dir.join("a.txt")),
            Chg::Modified(dir.join("b.txt"))
        ]
    );
}

#[tokio::test]
async fn test_remove_other_emits_deleted_dir() {
    let path = Path::new("/tmp/deleted.txt");
    assert_eq!(
        remove_changes(RemoveKind::Other, path).await,
        [Chg::DeletedDir(path.to_path_buf())]
    );
}

#[tokio::test]
async fn test_remove_file_emits_deleted() {
    let path = Path::new("/tmp/deleted.txt");
    assert_eq!(
        remove_changes(RemoveKind::File, path).await,
        [Chg::Deleted(path.to_path_buf())]
    );
}

#[tokio::test]
async fn test_remove_folder_emits_deleted_dir() {
    let path = Path::new("/tmp/deleted_dir");
    assert_eq!(
        remove_changes(RemoveKind::Folder, path).await,
        [Chg::DeletedDir(path.to_path_buf())]
    );
}

#[tokio::test]
async fn test_access_skipped() {
    let changes = event_changes(
        EventKind::Access(AccessKind::Any),
        vec![Path::new("/tmp/file.txt")],
        Path::new("/tmp"),
        Path::new(TMP_INDEX),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_rescan_flag_emits_rescan() {
    let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert_eq!(chgs(&changes), [Chg::Rescan]);
}

#[tokio::test]
async fn test_other_event_without_rescan_flag_emits_nothing() {
    let changes = event_changes(
        EventKind::Other,
        Vec::new(),
        Path::new("/tmp"),
        Path::new(TMP_INDEX),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_directory_create_skipped() {
    let dir = unique_temp_dir("watch_empty_dir_create");

    let changes = event_changes(
        EventKind::Create(CreateKind::Folder),
        vec![&dir],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_rename_from_emits_deleted_and_deleted_dir() {
    // RenameMode::From: type is always unknown — emit both variants
    let from_path = PathBuf::from("/tmp/old.txt");
    let changes = rename_changes(
        RenameMode::From,
        vec![&from_path],
        Path::new("/tmp"),
        Path::new(TMP_INDEX),
    )
    .await;
    assert_eq!(
        chgs(&changes),
        [Chg::Deleted(from_path.clone()), Chg::DeletedDir(from_path)]
    );
}

#[tokio::test]
async fn test_rename_to_emits_modified() {
    // RenameMode::To: new path exists — should emit Modified
    let dir = unique_temp_dir("watch_rename_to");
    let to_path = dir.join("new.txt");
    std::fs::write(&to_path, "renamed content").unwrap();

    let changes = rename_changes(RenameMode::To, vec![&to_path], &dir, &dir.join("index")).await;
    assert_eq!(chgs(&changes), [Chg::Modified(to_path)]);
}

#[tokio::test]
async fn test_rename_both_file_emits_delete_and_modified() {
    let dir = unique_temp_dir("watch_rename_both_file");
    let from_path = dir.join("old_name.txt");
    std::fs::write(&from_path, "content").unwrap();
    let to_path = dir.join("new_name.txt");
    std::fs::write(&to_path, "content").unwrap();

    let changes = rename_changes(
        RenameMode::Both,
        vec![&from_path, &to_path],
        &dir,
        &dir.join("index"),
    )
    .await;
    // Both delete variants are emitted (type is unknown), then Modified for the new path.
    assert_eq!(
        chgs(&changes),
        [
            Chg::Deleted(from_path.clone()),
            Chg::DeletedDir(from_path),
            Chg::Modified(to_path),
        ]
    );
}

#[tokio::test]
async fn test_rename_both_dir_emits_deletedir_and_modified() {
    let dir = unique_temp_dir("watch_rename_both_dir");
    let from_path = dir.join("old_dir");
    std::fs::create_dir_all(&from_path).unwrap();
    let to_path = dir.join("new_dir");
    std::fs::create_dir_all(&to_path).unwrap();
    // add_as_modified scans the 'to' dir for files — seed it so modified events are emitted.
    std::fs::write(to_path.join("inner.txt"), "data").unwrap();

    let changes = rename_changes(
        RenameMode::Both,
        vec![&from_path, &to_path],
        &dir,
        &dir.join("index"),
    )
    .await;
    // Both delete variants are emitted (type is unknown), then Modified for the scanned file.
    assert_eq!(
        chgs(&changes),
        [
            Chg::Deleted(from_path.clone()),
            Chg::DeletedDir(from_path),
            Chg::Modified(to_path.join("inner.txt")),
        ]
    );
}

#[tokio::test]
async fn test_rename_both_missing_parent_emits_both_deletes() {
    // The old path (and its parent) is gone — emit both Deleted and DeletedDir.
    let dir = unique_temp_dir("watch_rename_both_unknown");
    let from_path = PathBuf::from("/nonexistent/parent/old.txt");
    let to_path = dir.join("actual_new.txt");
    std::fs::write(&to_path, "content").unwrap();

    let changes = rename_changes(
        RenameMode::Both,
        vec![&from_path, &to_path],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert_eq!(
        chgs(&changes),
        [
            Chg::Deleted(from_path.clone()),
            Chg::DeletedDir(from_path),
            Chg::Modified(to_path),
        ]
    );
}

#[tokio::test]
async fn test_create_hidden_dir_emits_nothing() {
    // A hidden directory as walk root is skipped entirely: its contents
    // are never indexed, so no walk and no changes.
    let dir = unique_temp_dir("watch_hidden_root");
    let hidden = dir.join(".git");
    std::fs::create_dir_all(&hidden).unwrap();
    std::fs::write(hidden.join("config"), "data").unwrap();

    let changes = event_changes(
        EventKind::Create(CreateKind::Folder),
        vec![&hidden],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_create_dir_under_hidden_emits_nothing() {
    // A walk root *under* a hidden directory is caught by the
    // `should_skip_path` branch of `skip_directory_walk` (as opposed to
    // the hidden-root case above, caught by the name check).
    let dir = unique_temp_dir("watch_under_hidden");
    let objects = dir.join(".git").join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    std::fs::write(objects.join("pack"), "data").unwrap();

    let changes = event_changes(
        EventKind::Create(CreateKind::Folder),
        vec![&objects],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_create_dir_inside_index_emits_nothing() {
    // A folder created inside the index directory is skipped: no walk, no changes.
    let dir = unique_temp_dir("watch_index_walk");
    let index = dir.join("file_indexr_data");
    let sub = index.join("segments");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("0.seg"), "data").unwrap();

    let changes = event_changes(
        EventKind::Create(CreateKind::Folder),
        vec![&sub],
        &dir,
        &index,
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_rename_both_to_hidden_dir_keeps_pairing() {
    // 'to' is a hidden directory: its walk is skipped, but the (from, to)
    // pair must stay intact — only the two deletes for 'from' are
    // emitted, and the live 'to' path must not be treated as renamed-from.
    let dir = unique_temp_dir("watch_rename_hidden");
    let to_dir = dir.join(".cache");
    std::fs::create_dir_all(&to_dir).unwrap();
    std::fs::write(to_dir.join("blob.bin"), "data").unwrap();
    let from_path = dir.join("a.txt");

    let changes = rename_changes(
        RenameMode::Both,
        vec![&from_path, &to_dir],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert_eq!(
        chgs(&changes),
        [Chg::Deleted(from_path.clone()), Chg::DeletedDir(from_path)]
    );
}

#[tokio::test]
async fn test_create_dot_file_emits_modified() {
    // Top-level dot files stay indexable — the hidden-dir-name check in
    // `skip_directory_walk` must not apply to regular files.
    let dir = unique_temp_dir("watch_dot_file");
    let file_path = dir.join(".env");
    std::fs::write(&file_path, "SECRET=1").unwrap();

    let changes = event_changes(
        EventKind::Create(CreateKind::File),
        vec![&file_path],
        &dir,
        &dir.join("index"),
    )
    .await;
    assert_eq!(chgs(&changes), [Chg::Modified(file_path)]);
}

#[test]
fn test_should_skip_path_inside_index_dir() {
    // Non-hidden index dir: a hidden name (e.g. `.file_indexr`) would also be
    // skipped by the hidden-component rule, masking the index-prefix rule.
    let watched_dir = Path::new("/home/user/docs");
    let index_path = Path::new("/home/user/docs/file_indexr_data");
    // Anti-masking guard: if the fixture name ever becomes hidden, the
    // assertions below would no longer isolate the index-prefix rule.
    assert!(!has_hidden_component(
        Path::new("/home/user/docs/file_indexr_data/data.mfd"),
        watched_dir
    ));
    for path in [
        "/home/user/docs/file_indexr_data/data.mfd",
        "/home/user/docs/file_indexr_data",
    ] {
        assert!(
            should_skip_path(Path::new(path), watched_dir, index_path),
            "{path} should be skipped"
        );
    }
}

#[test]
fn test_should_skip_path_outside_index_dir() {
    let watched_dir = Path::new("/home/user/docs");
    let index_path = Path::new("/home/user/docs/.file_indexr");
    for path in ["/home/user/docs/hello.txt", "/home/user/docs/sub/file.rs"] {
        assert!(
            !should_skip_path(Path::new(path), watched_dir, index_path),
            "{path} should not be skipped"
        );
    }
}

#[test]
fn test_should_skip_hidden_paths() {
    let watched_dir = Path::new("/home/user/docs");
    let index_path = Path::new("/home/user/docs/.file_indexr");
    for path in [
        "/home/user/docs/.git/config",
        "/home/user/docs/.venv/lib/site.py",
        "/home/user/docs/src/.cache/data.bin",
    ] {
        assert!(
            should_skip_path(Path::new(path), watched_dir, index_path),
            "{path} should be skipped"
        );
    }
}

#[tokio::test]
async fn test_collect_files_in_directory() {
    let dir = unique_temp_dir("collect_files");
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
    // Hidden subdirectory should be skipped
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join(".hidden/c.txt"), "c").unwrap();

    let files = collect_files_in_directory(&dir, &dir.join("index")).await;
    let paths: Vec<_> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap())
        .collect();

    assert!(paths.contains(&"a.txt"));
    assert!(paths.contains(&"b.txt"));
    assert!(
        !paths.contains(&"c.txt"),
        "Hidden dir contents should be excluded"
    );
    assert_eq!(files.len(), 2);
}

#[tokio::test]
async fn test_collect_files_empty_directory() {
    let dir = unique_temp_dir("collect_empty");

    let files = collect_files_in_directory(&dir, &dir.join("index")).await;
    assert!(files.is_empty());
}

#[tokio::test]
async fn test_collect_files_nonexistent_directory() {
    let files =
        collect_files_in_directory(Path::new("/nonexistent/path"), Path::new("/index")).await;
    assert!(files.is_empty());
}

#[tokio::test]
async fn test_collect_files_prunes_index_dir() {
    // An index-directory subdirectory is pruned during the walk.
    let dir = unique_temp_dir("collect_index_prune");
    let index = dir.join("file_indexr_data");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::write(index.join("0.seg"), "seg").unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();

    let files = collect_files_in_directory(&dir, &index).await;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], dir.join("a.txt"));
}

/// Drive `run_event_loop` with a bridge that optionally reports a notify
/// error before closing.
async fn run_loop_with(bridge_err: Option<notify::Error>) -> Result<()> {
    let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<Result<Event, notify::Error>>();
    let (tx, _rx) = mpsc::channel::<FileChange>(16);
    let thread = std::thread::spawn(|| ());

    if let Some(e) = bridge_err {
        bridge_tx.send(Err(e)).unwrap();
    }
    drop(bridge_tx);

    run_event_loop(
        bridge_rx,
        thread,
        PathBuf::from("/"),
        PathBuf::from("/index"),
        tx,
    )
    .await
}

#[tokio::test]
async fn test_run_event_loop_propagates_notify_error() {
    let result = run_loop_with(Some(notify::Error::generic("test error"))).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_run_event_loop_clean_exit_is_ok() {
    // Closing the bridge without an error is a clean exit.
    let result = run_loop_with(None).await;
    assert!(result.is_ok());
}
