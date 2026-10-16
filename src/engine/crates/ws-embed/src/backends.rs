//! Backend discovery and selection.
//!
//! Selection policy (never assumes a GPU):
//! `CUDA > Vulkan > CPU`, overridable with an explicit [`BackendPreference`].
//! An explicit preference that is not available falls back to CPU rather than
//! failing, so the CPU path is always reachable.

use std::fmt;
use std::str::FromStr;

/// The backend family a device belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Cuda,
    Vulkan,
    Cpu,
}

impl BackendKind {
    /// Map a ggml backend registry name (e.g. `"CUDA"`, `"Vulkan"`, `"CPU"`).
    pub fn from_registry_name(name: &str) -> Option<Self> {
        match name {
            "CUDA" => Some(Self::Cuda),
            "Vulkan" => Some(Self::Vulkan),
            "CPU" => Some(Self::Cpu),
            _ => None,
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        })
    }
}

/// User-configurable backend preference (`preferred_backend` config key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendPreference {
    #[default]
    Auto,
    Cuda,
    Vulkan,
    Cpu,
}

impl FromStr for BackendPreference {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "cuda" => Ok(Self::Cuda),
            "vulkan" => Ok(Self::Vulkan),
            "cpu" => Ok(Self::Cpu),
            other => Err(format!("unknown backend preference: {other}")),
        }
    }
}

/// Pick the backend to use from the set of kinds actually available.
///
/// `available` lists every kind with at least one usable device.
/// - `Auto` returns the first of CUDA, Vulkan, CPU that is available.
/// - An explicit preference returns that kind when available, otherwise CPU.
pub fn select_backend(pref: BackendPreference, available: &[BackendKind]) -> BackendKind {
    let has = |k: BackendKind| available.contains(&k);
    match pref {
        BackendPreference::Auto => {
            if has(BackendKind::Cuda) {
                BackendKind::Cuda
            } else if has(BackendKind::Vulkan) {
                BackendKind::Vulkan
            } else {
                BackendKind::Cpu
            }
        }
        BackendPreference::Cuda => {
            if has(BackendKind::Cuda) {
                BackendKind::Cuda
            } else {
                BackendKind::Cpu
            }
        }
        BackendPreference::Vulkan => {
            if has(BackendKind::Vulkan) {
                BackendKind::Vulkan
            } else {
                BackendKind::Cpu
            }
        }
        BackendPreference::Cpu => BackendKind::Cpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_cuda_then_vulkan_then_cpu() {
        use BackendKind::*;
        assert_eq!(
            select_backend(BackendPreference::Auto, &[Cpu, Vulkan, Cuda]),
            Cuda
        );
        assert_eq!(select_backend(BackendPreference::Auto, &[Cpu, Vulkan]), Vulkan);
        assert_eq!(select_backend(BackendPreference::Auto, &[Cpu]), Cpu);
        assert_eq!(select_backend(BackendPreference::Auto, &[]), Cpu);
    }

    #[test]
    fn explicit_preference_falls_back_to_cpu_when_missing() {
        use BackendKind::*;
        assert_eq!(select_backend(BackendPreference::Cuda, &[Cpu, Vulkan]), Cpu);
        assert_eq!(select_backend(BackendPreference::Vulkan, &[Cpu]), Cpu);
        assert_eq!(select_backend(BackendPreference::Vulkan, &[Cpu, Vulkan]), Vulkan);
    }

    #[test]
    fn cpu_preference_ignores_gpus() {
        use BackendKind::*;
        assert_eq!(select_backend(BackendPreference::Cpu, &[Cuda, Vulkan, Cpu]), Cpu);
    }

    #[test]
    fn parses_preference_strings() {
        assert_eq!("auto".parse::<BackendPreference>().unwrap(), BackendPreference::Auto);
        assert_eq!(" CUDA ".parse::<BackendPreference>().unwrap(), BackendPreference::Cuda);
        assert_eq!("vulkan".parse::<BackendPreference>().unwrap(), BackendPreference::Vulkan);
        assert_eq!("cpu".parse::<BackendPreference>().unwrap(), BackendPreference::Cpu);
        assert!("metal".parse::<BackendPreference>().is_err());
    }

    #[test]
    fn maps_registry_names() {
        assert_eq!(BackendKind::from_registry_name("CUDA"), Some(BackendKind::Cuda));
        assert_eq!(BackendKind::from_registry_name("Vulkan"), Some(BackendKind::Vulkan));
        assert_eq!(BackendKind::from_registry_name("CPU"), Some(BackendKind::Cpu));
        assert_eq!(BackendKind::from_registry_name("BLAS"), None);
    }
}
