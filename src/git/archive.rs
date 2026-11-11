//! Exporting a commit's files as a zip or tar.gz archive (`git archive`).

use std::path::{Path, PathBuf};

use super::ops::GitCmd;

/// The kinds of archive `git archive` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    TarGz,
    Tar,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Zip, Format::TarGz, Format::Tar];

    /// The format a file name asks for by its extension.
    pub fn of(path: &Path) -> Option<Format> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        Format::ALL.into_iter().find(|f| f.extensions().iter().any(|ext| name.ends_with(&format!(".{ext}"))))
    }

    /// What `--format=` says.
    pub fn flag(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::TarGz => "tar.gz",
            Format::Tar => "tar",
        }
    }

    /// File name extensions, the usual one first.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Format::Zip => &["zip"],
            Format::TarGz => &["tar.gz", "tgz"],
            Format::Tar => &["tar"],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "Zip archive",
            Format::TarGz => "Tar archive, gzip-compressed",
            Format::Tar => "Tar archive",
        }
    }
}

/// The file to write and its format: an extension that names a format
/// decides; otherwise `fallback`'s extension is added to the name.
pub fn target(path: &Path, fallback: Format) -> (PathBuf, Format) {
    match Format::of(path) {
        Some(format) => (path.to_path_buf(), format),
        None => {
            let mut name = path.as_os_str().to_owned();
            name.push(format!(".{}", fallback.extensions()[0]));
            (PathBuf::from(name), fallback)
        }
    }
}

/// The folder an archive's files sit in: `<repository>-<what>`, with the
/// slashes of a branch name made dashes ("app-feature-login").
pub fn prefix(repository: &str, what: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '-' } else { c }).collect::<String>();
    format!("{}-{}/", clean(repository), clean(what))
}

/// `git archive` of `treeish` (a commit, branch or tag; `shown` names it)
/// into `file`, its files in the folder `prefix`.
pub fn export(treeish: &str, shown: &str, file: &Path, format: Format, prefix: &str) -> GitCmd {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let format_flag = format!("--format={}", format.flag());
    let prefix_flag = format!("--prefix={prefix}");
    let output = file.display().to_string();
    GitCmd::new(format!("Export {shown} as an archive"), &["archive", &format_flag, &prefix_flag, "-o", &output, treeish])
        .done(format!("Exported {shown} to {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    #[test]
    fn the_file_name_picks_the_format() {
        assert_eq!(Format::of(Path::new("out/app.ZIP")), Some(Format::Zip));
        assert_eq!(Format::of(Path::new("app-1.0.tar.gz")), Some(Format::TarGz));
        assert_eq!(Format::of(Path::new("app.tgz")), Some(Format::TarGz));
        assert_eq!(Format::of(Path::new("app.tar")), Some(Format::Tar));
        assert_eq!(Format::of(Path::new("app.1.0")), None);
        assert_eq!(target(Path::new("dir/app.1.0"), Format::TarGz), (PathBuf::from("dir/app.1.0.tar.gz"), Format::TarGz));
        assert_eq!(target(Path::new("dir/app.zip"), Format::TarGz), (PathBuf::from("dir/app.zip"), Format::Zip));
        assert_eq!(prefix("app", "feature/login"), "app-feature-login/");
    }

    #[test]
    fn a_commit_exports_as_zip_and_tar_gz() {
        let fx = Fixture::new("archive-export");
        fx.write("a.txt", "one\n");
        fx.write("dir/ünï file.txt", "two\n");
        fx.commit_all("base");
        git(&fx.dir, &["tag", "v1"]);
        fx.write("a.txt", "changed, not committed\n");
        let out = fx.dir.with_extension("archives");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();

        let zip = out.join("my app.zip");
        fx.run(export("v1", "v1", &zip, Format::Zip, &prefix("demo", "v1")));
        let bytes = std::fs::read(&zip).unwrap();
        assert_eq!(&bytes[..2], b"PK", "a zip file");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("demo-v1/a.txt") && text.contains("demo-v1/dir/"), "files in the prefix folder");

        let tgz = out.join("app.tar.gz");
        fx.run(export("HEAD", "main", &tgz, Format::TarGz, &prefix("demo", "main")));
        assert_eq!(&std::fs::read(&tgz).unwrap()[..2], &[0x1f, 0x8b], "gzip data");
    }

    #[test]
    fn a_ref_that_does_not_exist_fails() {
        let fx = Fixture::new("archive-missing");
        fx.write("a.txt", "one\n");
        fx.commit_all("base");
        let done = fx.run_as(
            export("no-such-branch", "no-such-branch", &fx.dir.with_extension("zip"), Format::Zip, "x/"),
            crate::worker::Origin::User,
        );
        assert!(done.result.is_err());
    }
}
