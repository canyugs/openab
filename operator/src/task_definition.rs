use crate::manifest::OABServiceManifest;

const DEFAULT_OPENAB_GUARDED_PORT: u16 = 18080;
const FALLBACK_OPENAB_GUARDED_PORT: u16 = 18081;

/// Network topology for an ECS Task's inbound webhook path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskIngressPlan {
    Direct {
        external_port: u16,
    },
    Guarded {
        external_port: u16,
        guard_image: String,
        openab_listen: String,
        guard_listen: String,
        guard_upstream: String,
    },
}

impl TaskIngressPlan {
    pub fn from_manifest(manifest: &OABServiceManifest) -> Option<Self> {
        let ingress = manifest.spec.ingress.as_ref()?;
        match &ingress.task_ingress_guard {
            Some(guard) => {
                let openab_port = if ingress.container_port == DEFAULT_OPENAB_GUARDED_PORT {
                    FALLBACK_OPENAB_GUARDED_PORT
                } else {
                    DEFAULT_OPENAB_GUARDED_PORT
                };
                Some(Self::Guarded {
                    external_port: ingress.container_port,
                    guard_image: guard.image.clone(),
                    openab_listen: format!("127.0.0.1:{openab_port}"),
                    guard_listen: format!("0.0.0.0:{}", ingress.container_port),
                    guard_upstream: format!("http://127.0.0.1:{openab_port}/webhook/line"),
                })
            }
            None => Some(Self::Direct {
                external_port: ingress.container_port,
            }),
        }
    }

    pub fn openab_environment(&self) -> Vec<(&'static str, &str)> {
        match self {
            Self::Direct { .. } => Vec::new(),
            Self::Guarded { openab_listen, .. } => {
                vec![("GATEWAY_LISTEN", openab_listen)]
            }
        }
    }

    pub fn guard_environment(&self) -> Option<Vec<(&'static str, &str)>> {
        match self {
            Self::Direct { .. } => None,
            Self::Guarded {
                guard_listen,
                guard_upstream,
                ..
            } => Some(vec![
                ("OPENAB_TASK_INGRESS_LISTEN", guard_listen),
                ("OPENAB_TASK_INGRESS_UPSTREAM", guard_upstream),
            ]),
        }
    }

    pub fn external_container_name(&self) -> &'static str {
        match self {
            Self::Direct { .. } => "openab",
            Self::Guarded { .. } => "task-ingress-guard",
        }
    }
}
