#![cfg(feature = "checkpoint")]
#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::AtomicUsize;
use loom::sync::Arc;
use loom::thread;

use std::path::PathBuf;
use std::sync::atomic::Ordering::SeqCst;

/// A fresh location for one test's checkpoints.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("checkpoint")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn builder(checkpoint: &std::path::Path) -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.checkpoint_file = Some(checkpoint.to_path_buf());
    // Checkpoint on every iteration.
    builder.checkpoint_interval = 1;
    builder
}

fn racy_model() {
    let a = Arc::new(AtomicUsize::new(0));
    let b = a.clone();

    let th = thread::spawn(move || b.store(1, SeqCst));
    a.load(SeqCst);

    th.join().unwrap();
}

/// A checkpoint left behind by one model must not be replayed against
/// another one: the recorded path does not fit it.
#[test]
fn checkpoint_of_another_model_is_ignored() {
    let file = scratch("foreign").join("checkpoint.json");

    builder(&file).check(racy_model);
    assert!(file.exists());

    // Not the model that wrote the checkpoint.
    builder(&file).check(|| {});
    builder(&file).check(|| {
        thread::spawn(|| {}).join().unwrap();
    });
}

/// Running the same model again picks its checkpoint up.
#[test]
fn checkpoint_of_the_same_model_is_resumed() {
    let file = scratch("same").join("checkpoint.json");

    builder(&file).check(racy_model);
    builder(&file).check(racy_model);
}

/// Pointed at a directory, every model gets a checkpoint file of its own.
#[test]
fn checkpoint_directory_has_a_file_per_model() {
    let dir = scratch("dir");

    builder(&dir).check(racy_model);
    builder(&dir).check(|| {
        thread::spawn(|| {}).join().unwrap();
    });

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();

    assert_eq!(files.len(), 2, "{:?}", files);
    assert!(
        files.iter().any(|name| name.contains("racy_model")),
        "{:?}",
        files
    );

    // And each can be resumed.
    builder(&dir).check(racy_model);
}

/// A path ending in a separator names a directory, even if it does not
/// exist yet.
#[test]
fn checkpoint_directory_is_created() {
    let dir = scratch("create").join("nested");
    let mut with_separator = dir.clone().into_os_string();
    with_separator.push("/");

    builder(std::path::Path::new(&with_separator)).check(racy_model);

    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}
