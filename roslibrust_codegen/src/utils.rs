use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    /// For now RosVersion is being left as an option, because our ability to detect the correct version is in question
    pub version: Option<RosVersion>,
}

impl PartialEq for Package {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.version == other.version
    }
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum RosVersion {
    ROS1,
    ROS2,
}

const CATKIN_IGNORE: &str = "CATKIN_IGNORE";
const PACKAGE_FILE_NAME: &str = "package.xml";
const ROS_PACKAGE_PATH_ENV_VAR: &str = "ROS_PACKAGE_PATH";

pub fn get_search_paths() -> Vec<PathBuf> {
    if let Ok(paths) = std::env::var(ROS_PACKAGE_PATH_ENV_VAR) {
        #[cfg(unix)]
        let separator = ":";
        #[cfg(windows)]
        let separator = ";";

        paths
            .split(separator)
            .map(PathBuf::from)
            .collect::<Vec<PathBuf>>()
    } else {
        log::warn!("No ROS_PACKAGE_PATH defined.");
        vec![]
    }
}

/// Finds ROS packages within a list of search paths.
///
/// This function may panic if it reaches a maximum search depth. If this function
/// panics while you're using it, you may have some infinite loop in your paths
/// due to symlinking.
pub fn crawl<P: AsRef<Path>>(search_paths: &[P]) -> Vec<Package> {
    let mut packages = vec![];

    for path in search_paths {
        const MAX_RECURSION_DEPTH: u16 = 1000;
        if let Ok(found_packages) =
            packages_from_path(path.as_ref().to_owned(), MAX_RECURSION_DEPTH)
        {
            packages = [packages, found_packages].concat();
        }
    }

    packages
}

fn packages_from_path(mut path: PathBuf, depth: u16) -> io::Result<Vec<Package>> {
    let mut found_packages = vec![];

    if depth == 0 {
        log::error!(
            "Reached depth limit in: {}. Possible symlink loop detected.",
            path.as_os_str().to_string_lossy()
        );
        return Err(io::ErrorKind::Other.into());
    }

    if path.as_path().is_dir() {
        // We have a valid path
        path.push(CATKIN_IGNORE);
        // We'll only check this directory if no CATKIN_IGNORE file is present
        // TODO: support for ament ignore and colcon ignore
        if !path.as_path().is_file() {
            assert!(path.pop());

            path.push(PACKAGE_FILE_NAME);
            if path.as_path().is_file() {
                // And there's a package.xml here!
                if let Ok((version, name)) = parse_ros_package_info(&path) {
                    // Remove package.xml from our path
                    assert!(path.pop());

                    log::debug!("Found package {name} at {}", path.display());

                    found_packages.push(Package {
                        name,
                        path,
                        version,
                    });
                }
            } else {
                // No file here, we'll have to go deeper
                assert!(path.pop());
                for subdir in std::fs::read_dir(path)
                    .unwrap()
                    .filter(|entry| match entry {
                        Ok(entry) => entry.path().as_path().is_dir(),
                        Err(_err) => false,
                    })
                    .map(|entry| entry.unwrap())
                {
                    found_packages = [
                        found_packages,
                        packages_from_path(subdir.path(), depth - 1)?,
                    ]
                    .concat()
                }
            }
        }
    } else {
        log::error!("{} is not a directory", path.to_string_lossy())
    }

    Ok(found_packages)
}

pub fn get_message_files(pkg: &Package) -> io::Result<Vec<PathBuf>> {
    Ok(message_files_from_path(pkg.path.as_path(), "msg")?
        .into_iter()
        .chain(message_files_from_path(pkg.path.as_path(), "srv")?.into_iter())
        .chain(message_files_from_path(pkg.path.as_path(), "action")?.into_iter())
        .collect())
}

fn message_files_from_path(path: &Path, ext: &str) -> io::Result<Vec<PathBuf>> {
    let mut msg_files = vec![];
    for entry in (std::fs::read_dir(path)?).flatten() {
        if entry.path().as_path().is_dir() {
            msg_files = [
                msg_files,
                message_files_from_path(entry.path().as_path(), ext)?,
            ]
            .concat()
        } else if entry.path().as_path().is_file() {
            if let Some(extension) = entry.path().extension() {
                if extension.to_str().unwrap() == ext {
                    msg_files.push(entry.path())
                }
            }
        }
    }

    Ok(msg_files)
}

pub fn deduplicate_packages(packages: Vec<Package>) -> Vec<Package> {
    fn package_name_fmt(pkg: &Package) -> String {
        format!(
            "{}_{}",
            pkg.name,
            match pkg.version {
                Some(RosVersion::ROS1) => "1",
                Some(RosVersion::ROS2) => "2",
                None => "unknown",
            }
        )
    }

    let mut package_map: HashMap<String, Package> = HashMap::new();
    for package in packages {
        if let Some(duplicate) = package_map.get(package.name.as_str()) {
            if &package == duplicate {
                log::warn!(
                    "Duplicate package found: {}. Discovered at paths: ({}, {})",
                    package.name,
                    duplicate.path.display(),
                    package.path.display()
                );
                log::warn!(
                    "Proceeding with the package found at the first path: {}",
                    duplicate.path.display()
                );
            } else {
                package_map.insert(package_name_fmt(&package), package);
            }
        } else {
            package_map.insert(package_name_fmt(&package), package);
        }
    }

    package_map.into_values().collect()
}

/// Parses a ROS package.xml file, which may be in any of the 3 supported formats,
/// and returns a tuple of (RosVersion, Package Name)
/// Note: the name of the folder the package resides in is NOT the name of the package,
/// although that is the convention.
/// Finding the name is considered infallible and panics if name cannot be determined
/// ROS version determination is heuristic only, and returns None if failed.
/// See: https://answers.ros.org/question/410017/how-to-determine-if-a-package-is-ros1-or-ros2/
fn parse_ros_package_info(
    path: impl AsRef<Path> + std::fmt::Debug,
) -> io::Result<(Option<RosVersion>, String)> {
    use std::fs::File;
    use std::io::BufReader;
    use xml::reader::{EventReader, ParserConfig, XmlEvent};
    const BUILD_TOOL_TAG: &str = "buildtool_depend";
    const NAME_TAG: &str = "name";

    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let parser = EventReader::new_with_config(
        reader,
        ParserConfig {
            trim_whitespace: true,
            ignore_comments: true,
            ..Default::default()
        },
    );

    let mut in_build = false;
    let mut in_name = false;
    let mut version = None;
    let mut name = None;
    for e in parser {
        match e {
            Ok(XmlEvent::StartElement { name, .. }) => {
                if name.local_name == BUILD_TOOL_TAG {
                    in_build = true;
                } else if name.local_name == NAME_TAG {
                    in_name = true;
                }
            }
            Ok(XmlEvent::EndElement { name, .. }) => {
                if name.local_name == BUILD_TOOL_TAG {
                    in_build = false;
                } else if name.local_name == NAME_TAG {
                    in_name = false;
                }
            }
            Ok(XmlEvent::Characters(data)) => {
                if in_build {
                    log::trace!("Got data inside of {BUILD_TOOL_TAG}: {data}");
                    match data.as_str() {
                        "catkin" => {
                            version = Some(RosVersion::ROS1);
                        }
                        "ament_cmake" => {
                            version = Some(RosVersion::ROS2);
                        }
                        _ => {}
                    };
                } else if in_name {
                    log::trace!("Got data inside of {NAME_TAG}: {data}");
                    name = Some(data);
                }
            }
            _ => {}
        }
    }

    if let Some(name) = name {
        Ok((version, name))
    } else {
        log::error!(
            "Failed to find the <name> tag within package.xml, which is a required tag: {path:?}"
        );
        Err(io::ErrorKind::Other.into())
    }
}

#[cfg(test)]
mod test {
    use crate::utils;

    #[test]
    fn verify_deduplicate_packages() {
        // Wow I am so upset, I thought I was going insane
        // std::Vec::dedup_by only removes *consecutive* elements that are equal
        let packages = vec![
            utils::Package {
                name: "diagnostic_msgs".into(),
                path: "/opt/ros/noetic/share/diagnostic_msgs".into(),
                version: Some(utils::RosVersion::ROS1),
            },
            utils::Package {
                name: "std_msgs".into(),
                path: "/tmp/std_msgs".into(),
                version: Some(utils::RosVersion::ROS1),
            },
            // This duplicate below should be removed
            utils::Package {
                name: "diagnostic_msgs".into(),
                path: "/code/assets/ros1_common_interfaces/common_msgs/diagnostic_msgs".into(),
                version: Some(utils::RosVersion::ROS1),
            },
            // This will be kept because the ROS Version is different
            utils::Package {
                name: "std_msgs".into(),
                path: "/ros2/std_msgs".into(),
                version: Some(utils::RosVersion::ROS2),
            },
        ];

        let deduplicated = utils::deduplicate_packages(packages);
        assert_eq!(deduplicated.len(), 3);
    }
}
