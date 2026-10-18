//! Pure parsers and attribution for container inventory snapshots.
//!
//! Runtime I/O and listing policy belong to callers; this module only turns
//! already-collected engine output into stable, typed rows.

use crate::sandbox::{
    CONTAINER_PREFIX, OwnedContainers, agent_container_name, container_name_for_config_profile,
    vpn_sidecar_name,
};
use crate::theme::StateRole;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Created,
    Running,
    Paused,
    Restarting,
    Exited,
    Dead,
    Removing,
    Unknown,
}

impl EngineState {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "created" => Self::Created,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "exited" => Self::Exited,
            "dead" => Self::Dead,
            "removing" => Self::Removing,
            _ => Self::Unknown,
        }
    }

    pub fn from_status(status: &str) -> Self {
        let s = status.trim().to_ascii_lowercase();
        if s.starts_with("up ") && s.contains("(paused)") {
            Self::Paused
        } else if s.starts_with("up ") {
            Self::Running
        } else if s.starts_with("exited (") {
            Self::Exited
        } else if s.starts_with("created") {
            Self::Created
        } else if s.starts_with("restarting") {
            Self::Restarting
        } else if s.starts_with("dead") {
            Self::Dead
        } else if s.starts_with("removal in progress") {
            Self::Removing
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    Unhealthy,
    Starting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMap {
    pub host_ip: Option<String>,
    pub host_port: Option<u16>,
    pub container_port: u16,
    pub range: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Ours,
    Unattributed,
    OtherProfile,
    Foreign,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Worktree,
    VpnSidecar,
    Agent,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRecord {
    pub id: String,
    pub names: Vec<String>,
    pub name: String,
    pub image: String,
    pub image_id: Option<String>,
    pub command: Option<String>,
    pub state: EngineState,
    pub status: String,
    pub health: Option<HealthState>,
    pub exit_code: Option<i32>,
    pub created: Option<i64>,
    pub started: Option<i64>,
    pub exited_at: Option<i64>,
    pub pid: Option<u32>,
    pub restarts: Option<u32>,
    pub labels: BTreeMap<String, String>,
    pub ports: Vec<PortMap>,
    pub networks: Vec<String>,
    pub pod: Option<String>,
    pub mounts: Vec<String>,
    pub backend: String,
    pub ownership: Ownership,
    pub role: Role,
    pub worktree: Option<String>,
}

impl ContainerRecord {
    pub fn short_id(&self) -> &str {
        self.id
            .char_indices()
            .nth(12)
            .map_or(self.id.as_str(), |(i, _)| &self.id[..i])
    }
    pub fn compose_project(&self) -> Option<&str> {
        self.labels
            .get("com.docker.compose.project")
            .map(String::as_str)
    }
    pub fn compose_service(&self) -> Option<&str> {
        self.labels
            .get("com.docker.compose.service")
            .map(String::as_str)
    }
    pub fn compose_container_number(&self) -> Option<u32> {
        self.labels
            .get("com.docker.compose.container-number")
            .and_then(|n| n.parse().ok())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<T> {
    pub rows: Vec<T>,
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectDetail {
    pub id: String,
    pub name: String,
    pub oci_runtime: Option<String>,
    pub restart_policy: Option<String>,
    pub restart_count: Option<u32>,
    pub oom_killed: bool,
    pub state: EngineState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    pub health: Option<HealthState>,
    pub failing_streak: Option<i32>,
    pub last_probe: Option<String>,
    pub ips: Vec<String>,
    pub cgroup_path: Option<String>,
    pub labels: BTreeMap<String, String>,
}

fn strv(v: &Value, key: &str) -> Option<&str> {
    v.get(key)?.as_str()
}
fn int(v: &Value, key: &str) -> Option<i64> {
    v.get(key)?.as_i64()
}
fn positive_time(v: Option<i64>) -> Option<i64> {
    v.filter(|n| *n > 0)
}
fn opt_text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn health(s: &str) -> Option<HealthState> {
    match s.to_ascii_lowercase().as_str() {
        "healthy" => Some(HealthState::Healthy),
        "unhealthy" => Some(HealthState::Unhealthy),
        "starting" => Some(HealthState::Starting),
        _ => None,
    }
}
fn labels_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}
fn names_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim_start_matches('/').to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
fn list_strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn array_ports(v: Option<&Value>) -> Vec<PortMap> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let port = p
                        .get("container_port")
                        .or_else(|| p.get("PrivatePort"))?
                        .as_u64()?;
                    Some(PortMap {
                        host_ip: opt_text(p.get("host_ip").or_else(|| p.get("IP"))),
                        host_port: p
                            .get("host_port")
                            .or_else(|| p.get("PublicPort"))
                            .and_then(Value::as_u64)
                            .and_then(|n| u16::try_from(n).ok())
                            .filter(|n| *n != 0),
                        container_port: u16::try_from(port).ok()?,
                        range: p
                            .get("range")
                            .and_then(Value::as_u64)
                            .and_then(|n| u16::try_from(n).ok())
                            .unwrap_or(1),
                        protocol: opt_text(p.get("protocol").or_else(|| p.get("Type")))
                            .unwrap_or_else(|| "tcp".into()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
struct RecordInput {
    id: String,
    names: Vec<String>,
    image: String,
    image_id: Option<String>,
    command: Option<String>,
    state: EngineState,
    status: String,
    created: Option<i64>,
    exit_code: Option<i32>,
    pid: Option<u32>,
    labels: BTreeMap<String, String>,
    ports: Vec<PortMap>,
    networks: Vec<String>,
    pod: Option<String>,
    restarts: Option<u32>,
    backend: String,
}

fn record(input: RecordInput) -> Option<ContainerRecord> {
    let name = input.names.first()?.clone();
    Some(ContainerRecord {
        id: input.id,
        names: input.names,
        name,
        image: input.image,
        image_id: input.image_id,
        command: input.command,
        state: input.state,
        status: input.status,
        health: None,
        exit_code: input.exit_code,
        created: positive_time(input.created),
        started: None,
        exited_at: None,
        pid: input.pid.filter(|n| *n != 0),
        restarts: input.restarts,
        labels: input.labels,
        ports: input.ports,
        networks: input.networks,
        pod: input.pod,
        mounts: Vec::new(),
        backend: input.backend,
        ownership: Ownership::Foreign,
        role: Role::None,
        worktree: None,
    })
}
fn doc_rows(json: &str) -> Result<Vec<Value>, ()> {
    serde_json::from_str(json).map_err(|_| ())
}

/// Parse podman 5 `ps -a --format json` and libpod list JSON.
pub fn parse_podman_ps_all(json: &str, backend: &str) -> Parsed<ContainerRecord> {
    let Ok(rows) = doc_rows(json) else {
        return Parsed {
            rows: Vec::new(),
            skipped: 1,
        };
    };
    let mut out = Parsed {
        rows: Vec::new(),
        skipped: 0,
    };
    for v in rows {
        let names = names_array(v.get("Names"));
        let ports = array_ports(v.get("Ports"));
        let command = v.get("Command").and_then(|c| {
            if let Some(s) = c.as_str() {
                Some(s.to_owned())
            } else {
                c.as_array().map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            }
        });
        let state = strv(&v, "State")
            .map(EngineState::parse)
            .unwrap_or_else(|| {
                strv(&v, "Status")
                    .map(EngineState::from_status)
                    .unwrap_or(EngineState::Unknown)
            });
        let row = strv(&v, "Id")
            .or_else(|| strv(&v, "ID"))
            .map(str::to_owned)
            .and_then(|id| {
                record(RecordInput {
                    id,
                    names,
                    image: opt_string(&v, "Image").unwrap_or_default(),
                    image_id: opt_text(v.get("ImageID")),
                    command,
                    state,
                    status: opt_string(&v, "Status").unwrap_or_default(),
                    created: int(&v, "Created"),
                    exit_code: int(&v, "ExitCode").and_then(|n| i32::try_from(n).ok()),
                    pid: int(&v, "Pid").and_then(|n| u32::try_from(n).ok()),
                    labels: labels_map(v.get("Labels")),
                    ports,
                    networks: list_strings(v.get("Networks")),
                    pod: opt_text(v.get("Pod")).or_else(|| opt_text(v.get("PodName"))),
                    restarts: int(&v, "Restarts").and_then(|n| u32::try_from(n).ok()),
                    backend: backend.to_owned(),
                })
            });
        if let Some(mut r) = row {
            r.mounts = list_strings(v.get("Mounts"));
            r.started = positive_time(int(&v, "StartedAt"));
            r.exited_at = positive_time(int(&v, "ExitedAt"));
            r.health = v
                .get("Health")
                .and_then(Value::as_str)
                .and_then(health)
                .or_else(|| {
                    v.get("HealthStatus")
                        .and_then(Value::as_str)
                        .and_then(health)
                });
            out.rows.push(r);
        } else {
            out.skipped += 1;
        }
    }
    out
}
fn opt_string(v: &Value, key: &str) -> Option<String> {
    opt_text(v.get(key))
}
fn parse_label_string(s: &str) -> BTreeMap<String, String> {
    s.split(',')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}
fn parse_text_ports(s: &str) -> Vec<PortMap> {
    s.split(", ")
        .filter_map(|item| {
            let (binding, proto) = item.split_once('/')?;
            let (host, inside) = binding
                .split_once("->")
                .map_or((None, binding), |(h, i)| (Some(h), i));
            let (container_port, range) =
                inside.split_once('-').map_or((inside, 1_u16), |(a, b)| {
                    (
                        a,
                        b.split('-')
                            .next()
                            .and_then(|n| n.parse::<u16>().ok())
                            .zip(a.parse::<u16>().ok())
                            .map(|(end, start)| end.saturating_sub(start).saturating_add(1))
                            .unwrap_or(1),
                    )
                });
            let container_port = container_port.parse::<u16>().ok()?;
            let (host_ip, host_port) =
                host.and_then(|h| h.rsplit_once(':'))
                    .map_or((None, None), |(ip, p)| {
                        (
                            Some(ip.trim_matches(['[', ']']).to_owned()),
                            p.split('-').next().and_then(|n| n.parse().ok()),
                        )
                    });
            Some(PortMap {
                host_ip,
                host_port,
                container_port,
                range,
                protocol: proto.to_owned(),
            })
        })
        .collect()
}
fn created_at(s: &str) -> Option<i64> {
    let value = s.get(..25)?;
    chrono::DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S %z")
        .ok()
        .map(|t| t.timestamp())
}
/// Parse Docker CLI NDJSON (`ps -a --no-trunc --format '{{json .}}'`).
pub fn parse_docker_ps_ndjson(text: &str, backend: &str) -> Parsed<ContainerRecord> {
    let mut out = Parsed {
        rows: Vec::new(),
        skipped: 0,
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            out.skipped += 1;
            continue;
        };
        let names = strv(&v, "Names")
            .map(|s| s.split(',').map(str::to_owned).collect())
            .unwrap_or_default();
        let status = opt_string(&v, "Status").unwrap_or_default();
        let state = strv(&v, "State")
            .map(EngineState::parse)
            .unwrap_or_else(|| EngineState::from_status(&status));
        let labels = strv(&v, "Labels")
            .map(parse_label_string)
            .unwrap_or_default();
        let networks = strv(&v, "Networks")
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let ports = strv(&v, "Ports").map(parse_text_ports).unwrap_or_default();
        let code = if state == EngineState::Exited {
            status
                .strip_prefix("Exited (")
                .and_then(|s| s.split(')').next())
                .and_then(|s| s.parse().ok())
        } else {
            None
        };
        let row = strv(&v, "ID")
            .or_else(|| strv(&v, "Id"))
            .map(str::to_owned)
            .and_then(|id| {
                record(RecordInput {
                    id,
                    names,
                    image: opt_string(&v, "Image").unwrap_or_default(),
                    image_id: opt_string(&v, "ImageID"),
                    command: opt_string(&v, "Command"),
                    state,
                    status,
                    created: created_at(strv(&v, "CreatedAt").unwrap_or("")),
                    exit_code: code,
                    pid: None,
                    labels,
                    ports,
                    networks,
                    pod: None,
                    restarts: None,
                    backend: backend.to_owned(),
                })
            });
        if let Some(r) = row {
            out.rows.push(r)
        } else {
            out.skipped += 1
        }
    }
    out
}
/// Parse Docker Engine `/containers/json?all=1` output.
pub fn parse_docker_api_list(json: &str, backend: &str) -> Parsed<ContainerRecord> {
    let Ok(rows) = doc_rows(json) else {
        return Parsed {
            rows: Vec::new(),
            skipped: 1,
        };
    };
    let mut out = Parsed {
        rows: Vec::new(),
        skipped: 0,
    };
    for v in rows {
        let names = names_array(v.get("Names"));
        let status = opt_string(&v, "Status").unwrap_or_default();
        let state = strv(&v, "State")
            .map(EngineState::parse)
            .unwrap_or_else(|| EngineState::from_status(&status));
        let networks = v
            .get("NetworkSettings")
            .and_then(|n| n.get("Networks"))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let row = strv(&v, "Id").map(str::to_owned).and_then(|id| {
            record(RecordInput {
                id,
                names,
                image: opt_string(&v, "Image").unwrap_or_default(),
                image_id: opt_string(&v, "ImageID"),
                command: opt_string(&v, "Command"),
                state,
                status,
                created: int(&v, "Created"),
                exit_code: None,
                pid: None,
                labels: labels_map(v.get("Labels")),
                ports: array_ports(v.get("Ports")),
                networks,
                pod: None,
                restarts: None,
                backend: backend.to_owned(),
            })
        });
        if let Some(r) = row {
            out.rows.push(r)
        } else {
            out.skipped += 1
        }
    }
    out
}
fn timestamp(v: Option<&Value>) -> Option<i64> {
    let s = v.and_then(Value::as_str)?;
    if s.starts_with("0001-01-01") {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}
fn inspect_health(state: &Value) -> (Option<HealthState>, Option<i32>, Option<String>) {
    let h = state.get("Health").or_else(|| state.get("Healthcheck"));
    let Some(h) = h else {
        return (None, None, None);
    };
    let status = h.get("Status").and_then(Value::as_str).and_then(health);
    let streak = h
        .get("FailingStreak")
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok());
    let probe = h
        .get("Log")
        .and_then(Value::as_array)
        .and_then(|a| a.last())
        .and_then(|e| e.get("Output"))
        .and_then(Value::as_str)
        .map(|s| s.chars().take(60).collect());
    (status, streak, probe)
}
fn inspect_ips(v: Option<&Value>) -> Vec<String> {
    let mut networks = v
        .and_then(|n| n.get("Networks"))
        .and_then(Value::as_object)
        .map(|networks| {
            networks
                .iter()
                .filter_map(|(name, network)| opt_string(network, "IPAddress").map(|ip| (name, ip)))
                .collect()
        })
        .unwrap_or_default();
    networks.sort_by(|(left, _), (right, _)| left.cmp(right));
    networks.into_iter().map(|(_, ip)| ip).collect()
}
/// Parse a batch inspect response from podman or Docker.
pub fn parse_inspect_batch(json: &str) -> Parsed<InspectDetail> {
    let Ok(rows) = doc_rows(json) else {
        return Parsed {
            rows: Vec::new(),
            skipped: 1,
        };
    };
    let mut out = Parsed {
        rows: Vec::new(),
        skipped: 0,
    };
    for v in rows {
        let state_obj = v.get("State").unwrap_or(&Value::Null);
        let name = opt_string(&v, "Name")
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned();
        let id = opt_string(&v, "Id").or_else(|| opt_string(&v, "ID"));
        let state = strv(state_obj, "Status")
            .map(EngineState::parse)
            .unwrap_or(EngineState::Unknown);
        let (health, failing_streak, last_probe) = inspect_health(state_obj);
        let mut ips = inspect_ips(
            state_obj
                .get("NetworkSettings")
                .or_else(|| v.get("NetworkSettings")),
        );
        let cfg = v.get("HostConfig").unwrap_or(&Value::Null);
        let runtime = opt_string(&v, "OCIRuntime").or_else(|| opt_string(cfg, "Runtime"));
        let restart = cfg.get("RestartPolicy").and_then(|r| opt_string(r, "Name"));
        let row = id.filter(|_| !name.is_empty()).map(|id| {
            ips.sort();
            InspectDetail {
                id,
                name,
                oci_runtime: runtime,
                restart_policy: restart,
                restart_count: int(&v, "RestartCount").and_then(|n| u32::try_from(n).ok()),
                oom_killed: state_obj
                    .get("OOMKilled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                state,
                pid: int(state_obj, "Pid")
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n != 0),
                exit_code: int(state_obj, "ExitCode").and_then(|n| i32::try_from(n).ok()),
                started: timestamp(state_obj.get("StartedAt")),
                finished: timestamp(state_obj.get("FinishedAt")),
                health,
                failing_streak,
                last_probe,
                ips,
                cgroup_path: opt_text(state_obj.get("CgroupPath")),
                labels: labels_map(
                    v.get("Config")
                        .and_then(|c| c.get("Labels"))
                        .or_else(|| v.get("Labels")),
                ),
            }
        });
        if let Some(r) = row {
            out.rows.push(r)
        } else {
            out.skipped += 1
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventAction {
    Create,
    Start,
    Die,
    Kill,
    Oom,
    Restart,
    Remove,
    HealthStatus(Option<HealthState>),
    Other(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineEvent {
    pub id: String,
    pub name: String,
    pub action: EventAction,
    pub time: i64,
    pub exit_code: Option<i32>,
}
fn event_action(s: &str) -> EventAction {
    match s.to_ascii_lowercase().as_str() {
        "create" => EventAction::Create,
        "start" => EventAction::Start,
        "die" | "died" => EventAction::Die,
        "kill" => EventAction::Kill,
        "oom" => EventAction::Oom,
        "restart" => EventAction::Restart,
        "remove" => EventAction::Remove,
        _ if s.starts_with("health_status:") => {
            EventAction::HealthStatus(s.split_once(':').and_then(|(_, h)| health(h.trim())))
        }
        _ => EventAction::Other(s.to_owned()),
    }
}
/// Parse one podman or Docker container event line.
pub fn parse_event_line(line: &str) -> Option<EngineEvent> {
    let v = serde_json::from_str::<Value>(line).ok()?;
    if let Some(kind) = strv(&v, "Type")
        && kind != "container"
    {
        return None;
    }
    let actor = v.get("Actor").unwrap_or(&Value::Null);
    let id = strv(&v, "ID").or_else(|| strv(actor, "ID"))?;
    let name = strv(&v, "Name")
        .or_else(|| actor.get("Attributes").and_then(|a| strv(a, "name")))
        .unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let raw = strv(&v, "Action").or_else(|| strv(&v, "Status"))?;
    let action = if raw.eq_ignore_ascii_case("health_status") {
        EventAction::HealthStatus(strv(&v, "HealthStatus").and_then(health))
    } else {
        event_action(raw)
    };
    let attrs = actor.get("Attributes").unwrap_or(&Value::Null);
    let exit_code = strv(attrs, "exitCode")
        .and_then(|s| s.parse().ok())
        .or_else(|| int(&v, "ContainerExitCode").and_then(|n| i32::try_from(n).ok()));
    let time = int(&v, "time").or_else(|| int(&v, "Time"))?;
    Some(EngineEvent {
        id: id.to_owned(),
        name: name.to_owned(),
        action,
        time,
        exit_code,
    })
}
/// Parse newline-delimited podman or Docker events.
pub fn parse_events(text: &str) -> Parsed<EngineEvent> {
    let mut out = Parsed {
        rows: Vec::new(),
        skipped: 0,
    };
    for line in text.lines() {
        match parse_event_line(line) {
            Some(e) => out.rows.push(e),
            None => out.skipped += 1,
        }
    }
    out
}

pub fn state_role(state: EngineState, health: Option<HealthState>) -> StateRole {
    match state {
        EngineState::Running if health == Some(HealthState::Unhealthy) => StateRole::Crit,
        EngineState::Dead => StateRole::Crit,
        EngineState::Restarting => StateRole::Warn,
        EngineState::Running if health == Some(HealthState::Starting) => StateRole::Warn,
        EngineState::Running => StateRole::Ok,
        EngineState::Created
        | EngineState::Paused
        | EngineState::Exited
        | EngineState::Removing => StateRole::Stale,
        EngineState::Unknown => StateRole::Unknown,
    }
}

/// Exact profile, backend, and deterministic-name ownership plus role lookup.
pub struct Attribution {
    owned: OwnedContainers,
    roles: BTreeMap<String, (Role, String)>,
    owner_identity: Option<String>,
}
impl Attribution {
    pub fn new(
        profile: &str,
        worktrees: &[(&str, Option<&str>)],
        owner_identity: Option<&str>,
    ) -> Self {
        let owned = OwnedContainers::new(profile, worktrees.iter().copied());
        let mut roles = BTreeMap::new();
        for (path, _) in worktrees {
            let base = container_name_for_config_profile(path, profile);
            roles.insert(base.clone(), (Role::Worktree, (*path).to_owned()));
            roles.insert(
                vpn_sidecar_name(&base),
                (Role::VpnSidecar, (*path).to_owned()),
            );
            roles.insert(
                agent_container_name(&base),
                (Role::Agent, (*path).to_owned()),
            );
        }
        Self {
            owned,
            roles,
            owner_identity: owner_identity.map(str::to_owned),
        }
    }
    pub fn attribute(&self, row: &mut ContainerRecord) {
        row.worktree = None;
        row.role = Role::None;
        if self.owned.owns(&row.name, &row.backend) {
            row.ownership = Ownership::Ours;
            if let Some((role, path)) = self.roles.get(&row.name) {
                row.role = *role;
                row.worktree = Some(path.clone())
            }
            return;
        }
        match (
            row.name.starts_with(CONTAINER_PREFIX),
            row.labels.get("thegn.owner"),
            self.owner_identity.as_deref(),
        ) {
            (true, Some(owner), Some(ours)) if owner == ours => {
                row.ownership = Ownership::Unattributed
            }
            (_, Some(owner), Some(ours)) if owner != ours => {
                row.ownership = Ownership::OtherProfile
            }
            _ => row.ownership = Ownership::Foreign,
        }
    }
    pub fn attribute_all(&self, rows: &mut [ContainerRecord]) {
        for row in rows {
            self.attribute(row)
        }
    }
}

#[cfg(test)]
#[path = "sandbox_inventory_tests.rs"]
mod tests;
