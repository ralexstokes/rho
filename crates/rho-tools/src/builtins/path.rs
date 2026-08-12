use std::{io, path::PathBuf};

pub(super) fn absolute(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(normalize(path))
    } else {
        Ok(normalize(std::env::current_dir()?.join(path)))
    }
}

pub(super) fn resolve(cwd: &std::path::Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(cwd.join(path))
    }
}

fn normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.parent().is_some() {
                    normalized.pop();
                }
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_lexically_resolved_from_the_working_directory() {
        assert_eq!(
            resolve(
                std::path::Path::new("/workspace/project"),
                "src/../Cargo.toml"
            ),
            PathBuf::from("/workspace/project/Cargo.toml")
        );
        assert_eq!(
            resolve(std::path::Path::new("/workspace/project"), "/tmp/file"),
            PathBuf::from("/tmp/file")
        );
        assert_eq!(
            resolve(
                std::path::Path::new("/workspace/project"),
                "../../../tmp/file"
            ),
            PathBuf::from("/tmp/file")
        );
    }
}
