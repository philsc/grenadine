//! Files of any kind that go with prompts, which the agent reads from disk
//! with its own tools. Unlike images, which go in the prompt itself, each
//! is uploaded on its own as soon as it's attached, to
//! `<uploads>/<id>/<name>`. Sending the prompt claims its uploads: they
//! move to `<attachments>/<session>/<id>/<name>`. Uploads that no prompt
//! claims are swept after `UPLOAD_TTL`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow};
use futures::{Stream, StreamExt as _};
use grenadine_core::api::{Attachment, Upload};
use tokio::io::AsyncWriteExt as _;

/// The most files one prompt may come with.
pub const MAX_ATTACHMENTS: usize = 20;

/// The largest file that may be uploaded, in bytes.
pub const MAX_UPLOAD_BYTES: u64 = 1 << 30;

/// How long an upload waits for a prompt to claim it.
pub const UPLOAD_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How often the uploads are swept.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The longest name a file is stored under, in bytes; most filesystems
/// take at most 255.
const MAX_NAME_BYTES: usize = 200;

/// `name` made safe to store a file under: its last path component,
/// without control characters and at most `MAX_NAME_BYTES` long, keeping
/// its extension. `None` when nothing usable is left.
pub fn safe_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() || clean == "." || clean == ".." {
        return None;
    }
    if clean.len() <= MAX_NAME_BYTES {
        return Some(clean.to_owned());
    }
    // Shortened from the end of the stem, so the extension stays.
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, format!(".{ext}")),
        _ => (clean, String::new()),
    };
    let mut end = MAX_NAME_BYTES - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}{ext}", &stem[..end]))
}

/// Whether `id` could be an upload's id.
pub fn valid_upload_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Stores `body` in `uploads` as a new upload of the file `name`, which
/// `safe_name` made safe. `None` when it's bigger than `MAX_UPLOAD_BYTES`;
/// nothing is left behind then, or when it fails.
pub async fn store<S, E>(uploads: &Path, name: &str, body: S) -> Result<Option<Upload>>
where
    S: Stream<Item = Result<axum::body::Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut body = std::pin::pin!(body);
    let id = crate::agent::random_hex(16);
    let dir = uploads.join(&id);
    tokio::fs::create_dir_all(&dir).await?;
    let stored = async {
        let mut file = tokio::fs::File::create(dir.join(name)).await?;
        let mut size = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            size += chunk.len() as u64;
            if size > MAX_UPLOAD_BYTES {
                return Ok(None);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(Some(Upload {
            id: id.clone(),
            name: name.to_owned(),
            size,
        }))
    }
    .await;
    if !matches!(stored, Ok(Some(_))) {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    stored
}

/// The upload `id` in `uploads`, if it's waiting for a prompt to claim it.
pub fn upload(uploads: &Path, id: &str) -> Option<Upload> {
    if !valid_upload_id(id) {
        return None;
    }
    let entry = std::fs::read_dir(uploads.join(id)).ok()?.flatten().next()?;
    let meta = entry.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(Upload {
        id: id.to_owned(),
        name: entry.file_name().into_string().ok()?,
        size: meta.len(),
    })
}

/// Deletes the upload `id`; `false` when there's none.
pub async fn delete(uploads: &Path, id: &str) -> Result<bool> {
    if !valid_upload_id(id) {
        return Ok(false);
    }
    match tokio::fs::remove_dir_all(uploads.join(id)).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Why the uploads `ids` can't go with a prompt, if they can't.
pub fn check(uploads: &Path, ids: &[String]) -> Option<String> {
    if ids.len() > MAX_ATTACHMENTS {
        return Some(format!(
            "a prompt can have at most {MAX_ATTACHMENTS} attachments"
        ));
    }
    for (i, id) in ids.iter().enumerate() {
        if ids[..i].contains(id) {
            return Some("a file is attached twice".into());
        }
        if upload(uploads, id).is_none() {
            return Some(format!(
                "the upload {id} is gone, e.g. because it expired; attach the file again"
            ));
        }
    }
    None
}

/// Moves the uploads `ids` from `uploads` to the session's attachments in
/// `attachments`, and returns them.
pub async fn claim(
    uploads: &Path,
    attachments: &Path,
    session: &str,
    ids: &[String],
) -> Result<Vec<Attachment>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let dir = attachments.join(session);
    tokio::fs::create_dir_all(&dir).await?;
    let mut claimed = Vec::new();
    for id in ids {
        let up = upload(uploads, id).ok_or_else(|| anyhow!("the upload {id} is gone"))?;
        tokio::fs::rename(uploads.join(id), dir.join(id)).await?;
        claimed.push(Attachment {
            dir: up.id,
            name: up.name,
            size: up.size,
        });
    }
    Ok(claimed)
}

/// The file of the session's attachment `dir`/`name` in `attachments`, if
/// it could be one.
pub fn file(attachments: &Path, session: &str, dir: &str, name: &str) -> Option<PathBuf> {
    let ok = crate::agent::valid_id(session)
        && valid_upload_id(dir)
        && safe_name(name).as_deref() == Some(name);
    ok.then(|| attachments.join(session).join(dir).join(name))
}

/// What the agent is told about the files attached to a prompt, which are
/// the session's in `attachments`.
pub fn note(attachments: &Path, session: &str, files: &[Attachment]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let lines: Vec<String> = files
        .iter()
        .map(|f| {
            let path = attachments.join(session).join(&f.dir).join(&f.name);
            format!("- {} ({})", path.display(), human_size(f.size))
        })
        .collect();
    Some(format!(
        "The user attached these files to this message. Read them with your tools as \
         needed:\n{}",
        lines.join("\n")
    ))
}

/// `bytes` for people, e.g. `1.2 MB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// Removes the uploads in `uploads` made longer than `ttl` before `now`,
/// and returns how many.
pub fn sweep(uploads: &Path, ttl: Duration, now: SystemTime) -> usize {
    let Ok(dirs) = std::fs::read_dir(uploads) else {
        return 0;
    };
    let mut removed = 0;
    for dir in dirs.flatten() {
        let name = dir.file_name().to_string_lossy().into_owned();
        let old = dir
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > ttl));
        if valid_upload_id(&name) && old {
            match std::fs::remove_dir_all(dir.path()) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!("can't remove the upload {name}: {e}"),
            }
        }
    }
    removed
}

/// Sweeps `uploads` now and then every `SWEEP_INTERVAL`.
pub async fn sweep_loop(uploads: PathBuf) {
    loop {
        let dir = uploads.clone();
        match tokio::task::spawn_blocking(move || sweep(&dir, UPLOAD_TTL, SystemTime::now())).await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!("removed {n} expired uploads"),
            Err(e) => tracing::warn!("can't sweep the uploads: {e}"),
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(chunks: &[&[u8]]) -> impl Stream<Item = Result<axum::body::Bytes, std::io::Error>> {
        futures::stream::iter(
            chunks
                .iter()
                .map(|c| Ok(axum::body::Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            safe_name("C:\\Users\\me\\a b.txt").as_deref(),
            Some("a b.txt")
        );
        assert_eq!(safe_name(" a\u{0}\nb.log ").as_deref(), Some("ab.log"));
        assert_eq!(safe_name(".hidden").as_deref(), Some(".hidden"));
        assert_eq!(safe_name(""), None);
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("dir/"), None);
        assert_eq!(safe_name("\u{7}"), None);

        let long = format!("{}.tar.gz", "é".repeat(300));
        let short = safe_name(&long).unwrap();
        assert!(short.len() <= MAX_NAME_BYTES, "{}", short.len());
        assert!(short.ends_with("é.gz"), "{short}");
        let no_ext = "x".repeat(300);
        assert_eq!(safe_name(&no_ext).unwrap().len(), MAX_NAME_BYTES);
    }

    #[test]
    fn upload_ids() {
        assert!(valid_upload_id(&crate::agent::random_hex(16)));
        assert!(!valid_upload_id("../../../../../../../../etc/pass"));
        assert!(!valid_upload_id("ABCDEF0123456789ABCDEF0123456789"));
        assert!(!valid_upload_id("abc"));
    }

    #[tokio::test]
    async fn stores_checks_and_claims_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let (uploads, attachments) = (tmp.path().join("uploads"), tmp.path().join("attachments"));
        let up = store(&uploads, "notes.txt", body(&[b"hello ", b"world"]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((up.name.as_str(), up.size), ("notes.txt", 11));
        assert_eq!(upload(&uploads, &up.id), Some(up.clone()));
        assert_eq!(upload(&uploads, "0".repeat(32).as_str()), None);

        assert_eq!(check(&uploads, std::slice::from_ref(&up.id)), None);
        let twice = check(&uploads, &[up.id.clone(), up.id.clone()]).unwrap();
        assert!(twice.contains("twice"), "{twice}");
        let gone = check(&uploads, &["0".repeat(32)]).unwrap();
        assert!(gone.contains("gone"), "{gone}");
        let many = check(&uploads, &vec![up.id.clone(); MAX_ATTACHMENTS + 1]).unwrap();
        assert!(many.contains("at most"), "{many}");

        let session = crate::agent::uuid();
        let claimed = claim(
            &uploads,
            &attachments,
            &session,
            std::slice::from_ref(&up.id),
        )
        .await
        .unwrap();
        assert_eq!(
            claimed,
            [Attachment {
                dir: up.id.clone(),
                name: "notes.txt".into(),
                size: 11
            }]
        );
        // Claimed uploads are no longer waiting, and can't be claimed again.
        assert_eq!(upload(&uploads, &up.id), None);
        assert!(
            claim(
                &uploads,
                &attachments,
                &session,
                std::slice::from_ref(&up.id)
            )
            .await
            .is_err()
        );
        let path = file(&attachments, &session, &up.id, "notes.txt").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn refuses_and_cleans_up_too_big_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let chunk = vec![0u8; 1 << 20];
        let chunks: Vec<&[u8]> = vec![&chunk; 1025];
        assert_eq!(
            store(tmp.path(), "big.bin", body(&chunks)).await.unwrap(),
            None
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn deletes_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let up = store(tmp.path(), "a", body(&[b"a"]))
            .await
            .unwrap()
            .unwrap();
        assert!(delete(tmp.path(), &up.id).await.unwrap());
        assert!(!delete(tmp.path(), &up.id).await.unwrap());
        assert!(!delete(tmp.path(), "..").await.unwrap());
    }

    #[test]
    fn serves_only_attachment_paths() {
        let dir = Path::new("/a");
        let (session, upload) = (crate::agent::uuid(), crate::agent::random_hex(16));
        assert_eq!(
            file(dir, &session, &upload, "x.pdf"),
            Some(dir.join(&session).join(&upload).join("x.pdf"))
        );
        assert_eq!(file(dir, &session, &upload, ".."), None);
        assert_eq!(file(dir, &session, &upload, "a/b"), None);
        assert_eq!(file(dir, &session, "..", "x.pdf"), None);
        assert_eq!(file(dir, "..", &upload, "x.pdf"), None);
    }

    #[test]
    fn notes_list_the_files_with_their_paths_and_sizes() {
        let files = [
            Attachment {
                dir: "d1".into(),
                name: "report.pdf".into(),
                size: 1_258_291,
            },
            Attachment {
                dir: "d2".into(),
                name: "a.log".into(),
                size: 12,
            },
        ];
        assert_eq!(note(Path::new("/x"), "s", &[]), None);
        assert_eq!(
            note(Path::new("/x"), "s", &files).unwrap(),
            "The user attached these files to this message. Read them with your tools as \
             needed:\n- /x/s/d1/report.pdf (1.2 MB)\n- /x/s/d2/a.log (12 B)"
        );
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 << 30), "3.0 GB");
    }

    #[tokio::test]
    async fn sweeps_only_old_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let up = store(tmp.path(), "a", body(&[b"a"]))
            .await
            .unwrap()
            .unwrap();
        std::fs::create_dir(tmp.path().join("not-an-upload")).unwrap();
        let ttl = Duration::from_secs(60);
        assert_eq!(sweep(tmp.path(), ttl, SystemTime::now()), 0);
        let later = SystemTime::now() + Duration::from_secs(120);
        assert_eq!(sweep(tmp.path(), ttl, later), 1);
        assert_eq!(upload(tmp.path(), &up.id), None);
        assert!(tmp.path().join("not-an-upload").exists());
        assert_eq!(sweep(&tmp.path().join("missing"), ttl, later), 0);
    }
}
