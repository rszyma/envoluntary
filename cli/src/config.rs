use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use duct::cmd;
use path_clean::PathClean;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::constants::CLI_NAME;

pub fn print_path() -> anyhow::Result<()> {
    println!("{}", get_config_path(None)?.display());
    Ok(())
}

pub fn edit(
    provided_config_path: Option<&Path>,
    provided_editor_program: Option<&OsStr>,
) -> anyhow::Result<()> {
    let config_path = get_config_path(provided_config_path)?;
    let editor_program =
        provided_editor_program.ok_or(anyhow::anyhow!("Couldn't find $EDITOR for config."))?;
    cmd!(editor_program, config_path).start()?.wait()?;
    Ok(())
}

pub fn add_entry(
    provided_config_path: Option<&Path>,
    pattern: String,
    flake_reference: String,
    pattern_adjacent: Option<String>,
    impure: Option<bool>,
) -> anyhow::Result<()> {
    let entry = ConfigEntry {
        pattern: Regex::new(&pattern)?,
        pattern_adjacent: pattern_adjacent.and_then(|s| Regex::new(&s).ok()),
        config: Config {
            flake_reference,
            impure,
        },
    };
    let config_path = get_config_path(provided_config_path)?;
    let mut envoluntary_config = EnvoluntaryConfig::load(&config_path)?;
    if let Some(ref mut entries) = envoluntary_config.entries {
        entries.push(entry);
    } else {
        envoluntary_config.entries = Some(vec![entry]);
    }
    envoluntary_config.save(&config_path)?;
    Ok(())
}

pub fn print_matching_entries(
    provided_config_path: Option<&Path>,
    path: &Path,
) -> anyhow::Result<()> {
    let config_path = get_config_path(provided_config_path)?;
    let envoluntary_config = EnvoluntaryConfig::load(&config_path)?;
    println!(
        "{}",
        serde_json::to_string(&envoluntary_config.matching_entries(path)?)?
    );
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvoluntaryConfig {
    entries: Option<Vec<ConfigEntry>>,
}

impl EnvoluntaryConfig {
    pub fn load(config_path: &Path) -> anyhow::Result<Self> {
        if !config_path.exists() {
            return Ok(EnvoluntaryConfig::default());
        }

        let envoluntary_config = config::Config::builder()
            .add_source(config::File::from(config_path))
            .build()?
            .try_deserialize::<EnvoluntaryConfig>()?;

        Ok(envoluntary_config)
    }

    pub fn save(&self, config_path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(&self)?;
        fs::write(config_path, contents)?;

        Ok(())
    }

    pub fn matching_entries(&self, path: &Path) -> anyhow::Result<Vec<ConfigEntry>> {
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()?.join(path)
        }
        .clean();
        let candidates = candidate_paths_for_matching(&absolute_path, path);
        let matched_entries = self
            .entries
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|entry| {
                let pattern_match = candidates.iter().any(|candidate| {
                    let candidate_string = candidate.to_string_lossy();
                    let candidate_string_with_tilde = replace_home_with_tilde(&candidate_string);
                    path_is_match_with_or_without_home_tilde(
                        &candidate_string,
                        candidate_string_with_tilde.as_ref(),
                        &entry.pattern,
                    )
                });
                if let Some(pattern_adjacent) = &entry.pattern_adjacent
                    && pattern_match
                {
                    return candidates.iter().any(|candidate| {
                        find_adjacent_dir_entry_walking_up_file_hierarchy(
                            candidate.clone(),
                            pattern_adjacent,
                        )
                        .is_some()
                    });
                }
                pattern_match
            })
            .cloned()
            .collect();
        Ok(matched_entries)
    }
}

fn candidate_paths_for_matching(absolute_path: &Path, original_path: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![absolute_path.to_path_buf()];
    if let Ok(canonical) = absolute_path.canonicalize()
        && !candidates.contains(&canonical)
    {
        candidates.push(canonical);
    }
    if let Some(logical) = logical_path_candidate(absolute_path, original_path)
        && !candidates.contains(&logical)
    {
        candidates.push(logical.clone());
        if let Ok(canonical_logical) = logical.canonicalize()
            && !candidates.contains(&canonical_logical)
        {
            candidates.push(canonical_logical);
        }
    }
    candidates
}

fn logical_path_candidate(absolute_path: &Path, original_path: &Path) -> Option<PathBuf> {
    let pwd_string = env::var("PWD").ok()?;
    if pwd_string.is_empty() {
        return None;
    }
    let logical_absolute = if original_path.is_absolute() {
        PathBuf::from(pwd_string).clean()
    } else {
        PathBuf::from(pwd_string).join(original_path).clean()
    };
    if logical_absolute == *absolute_path {
        return None;
    }
    let logical_resolved = best_effort_canonical(&logical_absolute)?;
    let absolute_resolved = best_effort_canonical(absolute_path)?;
    if logical_resolved == absolute_resolved {
        Some(logical_absolute)
    } else {
        None
    }
}

fn best_effort_canonical(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }
    let mut current = path.to_path_buf();
    let mut remainder = Vec::new();
    loop {
        match current.file_name() {
            Some(file_name) => {
                remainder.push(file_name.to_os_string());
                let Some(parent) = current.parent() else {
                    return None;
                };
                current = parent.to_path_buf();
                if let Ok(canonical_parent) = current.canonicalize() {
                    let mut result = canonical_parent;
                    for component in remainder.iter().rev() {
                        result.push(component);
                    }
                    return Some(result);
                }
            }
            None => return current.canonicalize().ok(),
        }
    }
}

fn find_adjacent_dir_entry_walking_up_file_hierarchy(
    start_dir: PathBuf,
    pattern_adjacent: &Regex,
) -> Option<PathBuf> {
    start_dir.ancestors().find_map(|ancestor| {
        fs::read_dir(ancestor).ok().and_then(|read_dir| {
            read_dir.filter_map(Result::ok).find_map(|dir_entry| {
                let dir_entry_path = dir_entry.path();
                let dir_entry_path_string = dir_entry_path.to_string_lossy();
                let dir_entry_path_string_with_tilde =
                    replace_home_with_tilde(&dir_entry_path_string);
                if path_is_match_with_or_without_home_tilde(
                    dir_entry_path_string,
                    dir_entry_path_string_with_tilde,
                    pattern_adjacent,
                ) {
                    Some(dir_entry_path)
                } else {
                    None
                }
            })
        })
    })
}

fn path_is_match_with_or_without_home_tilde(
    path_string: impl AsRef<str>,
    path_string_with_tilde: Option<impl AsRef<str>>,
    pattern: &Regex,
) -> bool {
    pattern.is_match(path_string.as_ref())
        || path_string_with_tilde
            .as_ref()
            .map(|p| pattern.is_match(p.as_ref()))
            .unwrap_or_default()
}

fn replace_home_with_tilde(path_string: impl AsRef<str>) -> Option<String> {
    get_home_dir().ok().and_then(|home_path| {
        let home_path_string = String::from(home_path.to_string_lossy());
        if path_string.as_ref().starts_with(&home_path_string) {
            return Some(path_string.as_ref().replacen(&home_path_string, "~", 1));
        }
        None
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntry {
    #[serde(with = "serde_regex")]
    pub pattern: Regex,
    #[serde(with = "serde_regex", default)]
    pub pattern_adjacent: Option<Regex>,
    #[serde(flatten)]
    pub config: Config,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub flake_reference: String,
    pub impure: Option<bool>,
}

pub fn get_config_path(provided_config_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(config_path) = provided_config_path {
        return Ok(PathBuf::from(config_path));
    }
    Ok(get_config_home_dir()?.join(CLI_NAME).join("config.toml"))
}

fn get_config_home_dir() -> anyhow::Result<PathBuf> {
    if let Some(xdg_config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home));
    }

    let home = get_home_dir()?;
    Ok(home.join(".config"))
}

pub fn get_cache_dir(provided_cache_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(cache_dir) = provided_cache_dir {
        return Ok(PathBuf::from(cache_dir));
    }
    Ok(get_cache_home_dir()?.join(CLI_NAME))
}

fn get_cache_home_dir() -> anyhow::Result<PathBuf> {
    if let Some(xdg_cache_home) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg_cache_home));
    }

    let home = get_home_dir()?;
    Ok(home.join(".cache"))
}

fn get_home_dir() -> anyhow::Result<PathBuf> {
    env::home_dir().ok_or_else(|| anyhow::anyhow!("Couldn't find $HOME for config."))
}
