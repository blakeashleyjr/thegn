//! Embedded skill documents and the pure, marker-aware seed planner.
//!
//! The module owns no paths and performs no I/O. The host discovers user
//! packages, surveys destination files, and applies the operations returned by
//! [`plan_seed`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Maximum accepted `SKILL.md` size. Skills are prose recipes, not archives.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 8 * 1024;
const MAX_FRONTMATTER_LINES: usize = 32;
const MAX_NAME_BYTES: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Version written into managed skill markers.
pub const SHIPPING_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where a parsed skill came from. The origin is diagnostic metadata only and
/// is never interpreted as a filesystem path by core.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillSource {
    Embedded { origin: String },
    User { origin: String },
}

impl SkillSource {
    pub fn embedded(origin: impl Into<String>) -> Self {
        Self::Embedded {
            origin: origin.into(),
        }
    }

    pub fn user(origin: impl Into<String>) -> Self {
        Self::User {
            origin: origin.into(),
        }
    }

    pub fn origin(&self) -> &str {
        match self {
            Self::Embedded { origin } | Self::User { origin } => origin,
        }
    }
}

/// What must be configured for a skill to be useful.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SkillGate {
    Always,
    MergeQueue,
    Pipeline,
}

impl SkillGate {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::MergeQueue => "merge_queue",
            Self::Pipeline => "pipeline",
        }
    }

    pub const fn is_open(self, state: GateState) -> bool {
        match self {
            Self::Always => true,
            Self::MergeQueue => state.merge_queue_open,
            Self::Pipeline => state.pipeline_open,
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "always" => Ok(Self::Always),
            "merge_queue" => Ok(Self::MergeQueue),
            "pipeline" => Ok(Self::Pipeline),
            _ => Err(format!(
                "unknown gate {raw:?}; expected one of: always, merge_queue, pipeline"
            )),
        }
    }
}

impl fmt::Display for SkillGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle phase in which a skill may be seeded.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SeedPhase {
    Create,
    Startup,
    Explicit,
}

impl SeedPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Startup => "startup",
            Self::Explicit => "explicit",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "create" => Ok(Self::Create),
            "startup" => Ok(Self::Startup),
            "explicit" => Ok(Self::Explicit),
            _ => Err(format!(
                "unknown seed phase {raw:?}; expected one of: create, startup, explicit"
            )),
        }
    }
}

impl fmt::Display for SeedPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Host-projected state used by typed skill gates.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct GateState {
    pub merge_queue_open: bool,
    pub pipeline_open: bool,
}

/// One validated skill package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SkillDocument {
    pub name: String,
    pub description: String,
    pub harnesses: BTreeSet<String>,
    pub gate: SkillGate,
    pub when: BTreeSet<SeedPhase>,
    /// Opaque markdown after the closing frontmatter delimiter.
    pub body: String,
    pub source: SkillSource,
}

/// A bounded frontmatter parse/validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError {
    pub source: SkillSource,
    pub line: Option<usize>,
    pub message: String,
}

impl SkillError {
    fn new(source: &SkillSource, line: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            source: source.clone(),
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source.origin())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for SkillError {}

/// Validate a skill/package name before any host path is built.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "skill name must be a path-safe segment of 1..={MAX_NAME_BYTES} bytes"
        ));
    }
    if name == "."
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(char::is_control)
        || name.starts_with('~')
    {
        return Err(format!(
            "skill name {name:?} is not a path-safe single segment"
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "skill name {name:?} is not path-safe; use ASCII letters, digits, `.`, `-`, or `_`"
        ));
    }
    Ok(())
}

/// Parse one bounded, flat-frontmatter skill document. `package_name` is the
/// already-observed immediate child directory name and must equal `name`.
pub fn parse_document(
    bytes: &[u8],
    package_name: &str,
    source: SkillSource,
) -> Result<SkillDocument, SkillError> {
    validate_name(package_name).map_err(|e| SkillError::new(&source, None, e))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(SkillError::new(
            &source,
            None,
            format!("document is larger than {MAX_DOCUMENT_BYTES} bytes"),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SkillError::new(&source, None, "document is not UTF-8"))?;
    let lines = lines_with_offsets(text);
    if lines.first().map(|(_, line)| *line) != Some("---") {
        return Err(SkillError::new(
            &source,
            Some(1),
            "document must start with a `---` frontmatter line",
        ));
    }

    let mut fields = BTreeMap::<&str, (&str, usize)>::new();
    let mut close = None;
    for (index, (offset, line)) in lines.iter().enumerate().skip(1) {
        let line_no = index + 1;
        if line_end_offset(text, *offset) > MAX_FRONTMATTER_BYTES || index > MAX_FRONTMATTER_LINES {
            return Err(SkillError::new(
                &source,
                Some(line_no),
                "frontmatter exceeds its bounded size",
            ));
        }
        if *line == "---" {
            close = Some((index, line_end_offset(text, *offset)));
            break;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            SkillError::new(
                &source,
                Some(line_no),
                "frontmatter entries must be flat `key: value` lines",
            )
        })?;
        let key = key.trim();
        let value = value.trim();
        if !matches!(key, "name" | "description" | "harnesses" | "gate" | "when") {
            return Err(SkillError::new(
                &source,
                Some(line_no),
                format!("unknown frontmatter key {key:?}"),
            ));
        }
        if value.is_empty() {
            return Err(SkillError::new(
                &source,
                Some(line_no),
                format!("frontmatter key {key:?} must not be empty"),
            ));
        }
        if fields.insert(key, (value, line_no)).is_some() {
            return Err(SkillError::new(
                &source,
                Some(line_no),
                format!("duplicate frontmatter key {key:?}"),
            ));
        }
    }
    let (_, body_offset) = close
        .ok_or_else(|| SkillError::new(&source, None, "frontmatter `---` fence is never closed"))?;

    let required = |key: &'static str| {
        fields.get(key).copied().ok_or_else(|| {
            SkillError::new(&source, None, format!("missing frontmatter key {key:?}"))
        })
    };
    let (name, name_line) = required("name")?;
    validate_name(name).map_err(|e| SkillError::new(&source, Some(name_line), e))?;
    if name != package_name {
        return Err(SkillError::new(
            &source,
            Some(name_line),
            format!("frontmatter name {name:?} must match package directory {package_name:?}"),
        ));
    }

    let (description, description_line) = required("description")?;
    if description.chars().count() > MAX_DESCRIPTION_CHARS
        || description.chars().any(char::is_control)
    {
        return Err(SkillError::new(
            &source,
            Some(description_line),
            format!(
                "description must be one non-control line of at most {MAX_DESCRIPTION_CHARS} characters"
            ),
        ));
    }

    let (raw_harnesses, harness_line) = required("harnesses")?;
    let harnesses = parse_csv(raw_harnesses, "harness", harness_line, &source, |id| {
        crate::harness::harness(id).is_some()
    })?;

    let (raw_gate, gate_line) = required("gate")?;
    let gate =
        SkillGate::parse(raw_gate).map_err(|e| SkillError::new(&source, Some(gate_line), e))?;

    let (raw_when, when_line) = required("when")?;
    let mut when = BTreeSet::new();
    for raw in csv_items(raw_when, "seed phase", when_line, &source)? {
        let phase =
            SeedPhase::parse(raw).map_err(|e| SkillError::new(&source, Some(when_line), e))?;
        if !when.insert(phase) {
            return Err(SkillError::new(
                &source,
                Some(when_line),
                format!("duplicate seed phase {raw:?}"),
            ));
        }
    }

    Ok(SkillDocument {
        name: name.to_string(),
        description: description.to_string(),
        harnesses,
        gate,
        when,
        body: text[body_offset..].to_string(),
        source,
    })
}

fn lines_with_offsets(text: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        lines.push((start, line.trim_end_matches(['\r', '\n'])));
    }
    lines
}

fn line_end_offset(text: &str, offset: usize) -> usize {
    text[offset..]
        .find('\n')
        .map_or(text.len(), |relative| offset + relative + 1)
}

fn csv_items<'a>(
    raw: &'a str,
    kind: &str,
    line: usize,
    source: &SkillSource,
) -> Result<Vec<&'a str>, SkillError> {
    let items: Vec<&str> = raw.split(',').map(str::trim).collect();
    if items.is_empty() || items.iter().any(|item| item.is_empty()) {
        return Err(SkillError::new(
            source,
            Some(line),
            format!("{kind} list must be non-empty and contain no empty entries"),
        ));
    }
    Ok(items)
}

fn parse_csv(
    raw: &str,
    kind: &str,
    line: usize,
    source: &SkillSource,
    known: impl Fn(&str) -> bool,
) -> Result<BTreeSet<String>, SkillError> {
    let mut out = BTreeSet::new();
    for item in csv_items(raw, kind, line, source)? {
        if !known(item) {
            return Err(SkillError::new(
                source,
                Some(line),
                format!("unknown {kind} {item:?}"),
            ));
        }
        if !out.insert(item.to_string()) {
            return Err(SkillError::new(
                source,
                Some(line),
                format!("duplicate {kind} {item:?}"),
            ));
        }
    }
    Ok(out)
}

/// Render the canonical, unmarked document whose bytes are content-hashed.
pub fn render_document(skill: &SkillDocument) -> String {
    let harnesses = skill
        .harnesses
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let when = skill
        .when
        .iter()
        .map(|phase| phase.as_str())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "---\nname: {}\ndescription: {}\nharnesses: {harnesses}\ngate: {}\nwhen: {when}\n---\n{}",
        skill.name, skill.description, skill.gate, skill.body
    )
}

/// SHA-256 of a canonical unmarked document.
pub fn document_hash(skill: &SkillDocument) -> String {
    sha256(render_document(skill).as_bytes())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Render an installable skill with the marker in the same frontmatter block
/// as the harness-readable metadata.
pub fn render_managed(skill: &SkillDocument) -> String {
    let unmanaged = render_document(skill);
    let hash = sha256(unmanaged.as_bytes());
    let marker =
        format!("thegn_managed: true\nthegn_version: {SHIPPING_VERSION}\nthegn_hash: {hash}\n");
    unmanaged.replacen("---\n", &format!("---\n{marker}"), 1)
}

/// Parsed proof that a destination file was written by thegn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManagedMarker {
    pub version: String,
    pub recorded_hash: String,
}

/// Marker proof plus the digest of the actual unmarked bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedFile {
    pub marker: ManagedMarker,
    pub actual_hash: String,
    pub unmarked: String,
}

impl ManagedFile {
    pub fn is_user_modified(&self) -> bool {
        self.marker.recorded_hash != self.actual_hash
    }
}

/// Inspect a managed rendering. Missing, partial, duplicate, or malformed
/// marker keys return `None`, making the file user-owned and untouchable.
pub fn inspect_managed(bytes: &[u8]) -> Option<ManagedFile> {
    let text = std::str::from_utf8(bytes).ok()?;
    let lines = lines_with_offsets(text);
    if lines.first().map(|(_, line)| *line) != Some("---") {
        return None;
    }
    let mut version = None;
    let mut recorded_hash = None;
    let mut managed = None;
    let mut remove_lines = BTreeSet::new();
    let mut close = None;
    for (index, (_, line)) in lines.iter().enumerate().skip(1) {
        if *line == "---" {
            close = Some(index);
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let slot = match key.trim() {
            "thegn_managed" => &mut managed,
            "thegn_version" => &mut version,
            "thegn_hash" => &mut recorded_hash,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return None;
        }
        remove_lines.insert(index);
    }
    let close = close?;
    if managed? != "true" {
        return None;
    }
    let version = version?;
    if version.is_empty() || version.len() > 128 || version.chars().any(char::is_control) {
        return None;
    }
    let recorded_hash = recorded_hash?;
    let digest = recorded_hash.strip_prefix("sha256:")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }

    let mut unmarked = String::new();
    for (index, (offset, _)) in lines.iter().enumerate().take(close + 1) {
        if !remove_lines.contains(&index) {
            let end = line_end_offset(text, *offset);
            unmarked.push_str(&text[*offset..end]);
        }
    }
    let body_offset = line_end_offset(text, lines[close].0);
    unmarked.push_str(&text[body_offset..]);
    let actual_hash = sha256(unmarked.as_bytes());
    Some(ManagedFile {
        marker: ManagedMarker {
            version: version.to_string(),
            recorded_hash: recorded_hash.to_string(),
        },
        actual_hash,
        unmarked,
    })
}

/// Deterministic registry. Insertion is first-wins, so callers load built-ins
/// before user packages and report duplicates without replacing trusted prose.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkillRegistry {
    pub skills: BTreeMap<String, SkillDocument>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, skill: SkillDocument) -> Result<(), String> {
        let errors = validate_document(&skill);
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        if self.skills.contains_key(&skill.name) {
            return Err(format!("duplicate skill name {:?}", skill.name));
        }
        self.skills.insert(skill.name.clone(), skill);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&SkillDocument> {
        self.skills.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SkillDocument)> {
        self.skills
            .iter()
            .map(|(name, skill)| (name.as_str(), skill))
    }

    pub fn embedded() -> Result<Self, SkillError> {
        let mut registry = Self::new();
        for entry in EMBEDDED_MANIFEST {
            let source = SkillSource::embedded(entry.origin);
            let skill = parse_document(entry.document.as_bytes(), entry.name, source.clone())?;
            registry
                .insert(skill)
                .map_err(|e| SkillError::new(&source, None, e))?;
        }
        Ok(registry)
    }
}

fn validate_document(skill: &SkillDocument) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = validate_name(&skill.name) {
        errors.push(error);
    }
    if skill.description.is_empty()
        || skill.description.chars().count() > MAX_DESCRIPTION_CHARS
        || skill.description.chars().any(char::is_control)
    {
        errors.push(format!(
            "skill {:?}: description must be one non-control line of 1..={MAX_DESCRIPTION_CHARS} characters",
            skill.name
        ));
    }
    if skill.harnesses.is_empty() {
        errors.push(format!(
            "skill {:?}: harnesses must not be empty",
            skill.name
        ));
    }
    for harness in &skill.harnesses {
        if crate::harness::harness(harness).is_none() {
            errors.push(format!(
                "skill {:?}: unknown harness {harness:?}",
                skill.name
            ));
        }
    }
    if skill.when.is_empty() {
        errors.push(format!(
            "skill {:?}: seed phases must not be empty",
            skill.name
        ));
    }
    if render_document(skill).len() > MAX_DOCUMENT_BYTES {
        errors.push(format!(
            "skill {:?}: rendered document is larger than {MAX_DOCUMENT_BYTES} bytes",
            skill.name
        ));
    }
    errors
}

/// Validate a registry even when a caller assembled its public map directly.
pub fn validate_registry(registry: &SkillRegistry) -> Vec<String> {
    let mut errors = Vec::new();
    for (key, skill) in &registry.skills {
        if key != &skill.name {
            errors.push(format!(
                "registry key {key:?} does not match document name {:?}",
                skill.name
            ));
        }
        errors.extend(validate_document(skill));
    }
    errors
}

/// One compile-time reviewed built-in.
pub struct EmbeddedManifestEntry {
    pub name: &'static str,
    pub origin: &'static str,
    pub document: &'static str,
}

/// The shipped manifest. `tui-check` is intentionally development-only.
pub const EMBEDDED_MANIFEST: &[EmbeddedManifestEntry] = &[
    EmbeddedManifestEntry {
        name: "mq",
        origin: "extensions/skills/mq/SKILL.md",
        document: include_str!("../../../extensions/skills/mq/SKILL.md"),
    },
    EmbeddedManifestEntry {
        name: "pipeline",
        origin: "extensions/skills/pipeline/SKILL.md",
        document: include_str!("../../../extensions/skills/pipeline/SKILL.md"),
    },
    EmbeddedManifestEntry {
        name: "supervise",
        origin: "extensions/skills/supervise/SKILL.md",
        document: include_str!("../../../extensions/skills/supervise/SKILL.md"),
    },
];

/// One surveyed target file, relative to a harness's project skill root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingFile {
    pub relative: String,
    pub bytes: Vec<u8>,
}

impl ExistingFile {
    pub fn new(relative: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            relative: relative.into(),
            bytes: bytes.into(),
        }
    }
}

/// Pure plan target. Vendor roots are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedTarget {
    pub harness: String,
    pub phase: SeedPhase,
    pub exclude: BTreeSet<String>,
}

impl SeedTarget {
    pub fn new(
        harness: impl Into<String>,
        phase: SeedPhase,
        exclude: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            harness: harness.into(),
            phase,
            exclude: exclude.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteReason {
    Absent,
    ChangedManaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoveReason {
    Excluded,
    Deprecated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOperation {
    pub relative: String,
    pub contents: Vec<u8>,
    pub reason: WriteReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOperation {
    pub relative: String,
    pub reason: RemoveReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub relative: String,
}

/// Deterministic result of comparing the registry with an abstract survey.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeedPlan {
    pub writes: Vec<WriteOperation>,
    pub unchanged: Vec<PlanEntry>,
    pub skipped_unmarked: Vec<PlanEntry>,
    pub skipped_adopted: Vec<PlanEntry>,
    pub removed_managed: Vec<RemoveOperation>,
    pub diagnostics: Vec<String>,
}

impl SeedPlan {
    /// Whether applying this plan performs no writes or removals.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.removed_managed.is_empty()
    }
}

/// The package-internal destination, after validating `name`.
pub fn skill_relative(name: &str) -> Result<String, String> {
    validate_name(name)?;
    Ok(format!("{name}/SKILL.md"))
}

fn relative_name(relative: &str) -> Option<&str> {
    let (name, file) = relative.split_once('/')?;
    (file == "SKILL.md" && !name.contains('/') && validate_name(name).is_ok()).then_some(name)
}

/// Build a pure marker-safe seed plan. Gate/phase/harness filtering never
/// removes an old file; only explicit exclusion or absence from the current
/// registry can do so, and then only with an intact marker/hash proof.
pub fn plan_seed(
    registry: &SkillRegistry,
    target: &SeedTarget,
    existing: &[ExistingFile],
    gates: GateState,
) -> SeedPlan {
    let mut plan = SeedPlan::default();
    if crate::harness::harness(&target.harness).is_none() {
        plan.diagnostics
            .push(format!("unknown harness {:?}", target.harness));
        return plan;
    }
    for name in &target.exclude {
        if let Err(error) = validate_name(name) {
            plan.diagnostics
                .push(format!("invalid excluded skill {name:?}: {error}"));
        }
    }

    let mut survey = BTreeMap::<&str, &ExistingFile>::new();
    for file in existing {
        if relative_name(&file.relative).is_none() {
            plan.diagnostics.push(format!(
                "ignored unsafe or unsupported skill-relative path {:?}",
                file.relative
            ));
            continue;
        }
        if survey.insert(&file.relative, file).is_some() {
            plan.diagnostics
                .push(format!("duplicate survey entry {:?}", file.relative));
        }
    }

    for (name, skill) in registry.iter() {
        let relative = skill_relative(name).expect("registry names were validated");
        let file = survey.get(relative.as_str()).copied();
        if target.exclude.contains(name) {
            classify_removal(file, &relative, RemoveReason::Excluded, &mut plan);
            continue;
        }
        if !skill.harnesses.contains(&target.harness)
            || !skill.gate.is_open(gates)
            || !skill.when.contains(&target.phase)
        {
            continue;
        }
        let desired = render_managed(skill);
        let desired_hash = document_hash(skill);
        match file {
            None => plan.writes.push(WriteOperation {
                relative,
                contents: desired.into_bytes(),
                reason: WriteReason::Absent,
            }),
            Some(file) => match inspect_managed(&file.bytes) {
                None => plan.skipped_unmarked.push(PlanEntry { relative }),
                Some(managed) if managed.is_user_modified() => {
                    plan.skipped_adopted.push(PlanEntry { relative });
                }
                Some(managed) if managed.marker.recorded_hash == desired_hash => {
                    plan.unchanged.push(PlanEntry { relative });
                }
                Some(_) => plan.writes.push(WriteOperation {
                    relative,
                    contents: desired.into_bytes(),
                    reason: WriteReason::ChangedManaged,
                }),
            },
        }
    }

    for (relative, file) in survey {
        let name = relative_name(relative).expect("survey was filtered");
        if !registry.skills.contains_key(name) {
            classify_removal(Some(file), relative, RemoveReason::Deprecated, &mut plan);
        }
    }
    plan
}

fn classify_removal(
    file: Option<&ExistingFile>,
    relative: &str,
    reason: RemoveReason,
    plan: &mut SeedPlan,
) {
    let Some(file) = file else { return };
    match inspect_managed(&file.bytes) {
        None => plan.skipped_unmarked.push(PlanEntry {
            relative: relative.to_string(),
        }),
        Some(managed) if managed.is_user_modified() => plan.skipped_adopted.push(PlanEntry {
            relative: relative.to_string(),
        }),
        Some(_) => plan.removed_managed.push(RemoveOperation {
            relative: relative.to_string(),
            reason,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str) -> SkillSource {
        SkillSource::user(format!("fixture/{name}/SKILL.md"))
    }

    fn doc(name: &str, gate: &str, when: &str) -> SkillDocument {
        let raw = format!(
            "---\nname: {name}\ndescription: fixture {name}\nharnesses: claude,codex,pi\ngate: {gate}\nwhen: {when}\n---\n\n# {name}\n"
        );
        parse_document(raw.as_bytes(), name, source(name)).unwrap()
    }

    fn registry(skills: Vec<SkillDocument>) -> SkillRegistry {
        let mut registry = SkillRegistry::new();
        for skill in skills {
            registry.insert(skill).unwrap();
        }
        registry
    }

    fn target(exclude: &[&str]) -> SeedTarget {
        SeedTarget::new(
            "claude",
            SeedPhase::Explicit,
            exclude.iter().map(|name| (*name).to_string()),
        )
    }

    #[test]
    fn path_safe_names_reject_prefixes_separators_and_controls() {
        for valid in ["mq", "pipeline-v2", "skill_1", "skill.v2", "A"] {
            assert_eq!(validate_name(valid), Ok(()), "{valid}");
        }
        for invalid in [
            "", ".", "..", "../x", "a/b", "a\\b", "C:tmp", "~root", "a b", "x\n",
        ] {
            assert!(validate_name(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn frontmatter_parser_is_bounded_and_strict() {
        let valid = doc("demo", "always", "create,startup,explicit");
        assert_eq!(valid.name, "demo");
        assert!(valid.body.contains("# demo"));
        for raw in [
            "name: demo\n",
            "---\nname: demo\n---\n",
            "---\nname: demo\nname: twice\ndescription: d\nharnesses: claude\ngate: always\nwhen: explicit\n---\n",
            "---\nname: demo\ndescription: d\nharnesses: claude\ngate: always\nwhen: explicit\nextra: nope\n---\n",
        ] {
            assert!(
                parse_document(raw.as_bytes(), "demo", source("demo")).is_err(),
                "{raw}"
            );
        }
        let huge = vec![b'x'; MAX_DOCUMENT_BYTES + 1];
        assert!(parse_document(&huge, "demo", source("demo")).is_err());
    }

    #[test]
    fn frontmatter_rejects_package_mismatch_unknown_harness_gate_and_phase() {
        let base = |h: &str, g: &str, w: &str| {
            format!(
                "---\nname: demo\ndescription: d\nharnesses: {h}\ngate: {g}\nwhen: {w}\n---\nbody\n"
            )
        };
        assert!(
            parse_document(
                base("claude", "always", "explicit").as_bytes(),
                "other",
                source("demo")
            )
            .is_err()
        );
        assert!(
            parse_document(
                base("gemini", "always", "explicit").as_bytes(),
                "demo",
                source("demo")
            )
            .is_err()
        );
        assert!(
            parse_document(
                base("claude", "sometimes", "explicit").as_bytes(),
                "demo",
                source("demo")
            )
            .is_err()
        );
        assert!(
            parse_document(
                base("claude", "always", "later").as_bytes(),
                "demo",
                source("demo")
            )
            .is_err()
        );
        assert!(
            parse_document(
                base("claude,claude", "always", "explicit").as_bytes(),
                "demo",
                source("demo")
            )
            .is_err()
        );
    }

    #[test]
    fn embedded_manifest_is_valid_deterministic_and_excludes_tui_check() {
        let registry = SkillRegistry::embedded().unwrap();
        let names: Vec<&str> = registry.iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["mq", "pipeline", "supervise"]);
        assert!(!registry.skills.contains_key("tui-check"));
        assert_eq!(registry.get("mq").unwrap().gate, SkillGate::MergeQueue);
        assert_eq!(registry.get("pipeline").unwrap().gate, SkillGate::Pipeline);
        assert_eq!(registry.get("supervise").unwrap().gate, SkillGate::Always);
        for (_, skill) in registry.iter() {
            assert_eq!(
                skill.harnesses,
                BTreeSet::from(["claude".into(), "codex".into(), "pi".into()])
            );
            assert_eq!(
                skill.when,
                BTreeSet::from([SeedPhase::Create, SeedPhase::Startup, SeedPhase::Explicit])
            );
            let source = EMBEDDED_MANIFEST
                .iter()
                .find(|entry| entry.name == skill.name)
                .unwrap();
            assert_eq!(
                render_document(skill),
                source.document,
                "{} canonical rendering changed its reviewed source",
                skill.name
            );
        }
        assert!(validate_registry(&registry).is_empty());
    }

    #[test]
    fn registry_is_first_wins_for_builtin_precedence() {
        let mut registry = registry(vec![doc("same", "always", "explicit")]);
        let mut duplicate = doc("same", "always", "explicit");
        duplicate.description = "external replacement".into();
        assert!(registry.insert(duplicate).is_err());
        assert_eq!(registry.get("same").unwrap().description, "fixture same");
    }

    #[test]
    fn registry_validation_catches_directly_constructed_invalid_documents() {
        let mut skill = doc("valid", "always", "explicit");
        skill.name = "../escape".into();
        skill.harnesses.insert("unknown".into());
        skill.when.clear();
        let registry = SkillRegistry {
            skills: BTreeMap::from([("wrong-key".into(), skill)]),
        };
        let errors = validate_registry(&registry);
        assert!(errors.iter().any(|e| e.contains("does not match")));
        assert!(errors.iter().any(|e| e.contains("path-safe")));
        assert!(errors.iter().any(|e| e.contains("unknown harness")));
        assert!(errors.iter().any(|e| e.contains("must not be empty")));
    }

    #[test]
    fn gates_phases_and_harness_targets_filter_without_removal() {
        let registry = registry(vec![
            doc("always", "always", "explicit"),
            doc("mq", "merge_queue", "explicit"),
            doc("pipe", "pipeline", "startup"),
        ]);
        let closed = plan_seed(&registry, &target(&[]), &[], GateState::default());
        assert_eq!(closed.writes.len(), 1);
        assert!(closed.writes[0].relative.starts_with("always/"));
        let open = plan_seed(
            &registry,
            &target(&[]),
            &[],
            GateState {
                merge_queue_open: true,
                pipeline_open: true,
            },
        );
        assert_eq!(open.writes.len(), 2, "startup-only skill stays filtered");
        let mut codex = target(&[]);
        codex.harness = "aider".into();
        assert!(plan_seed(&registry, &codex, &[], GateState::default()).is_empty());
    }

    #[test]
    fn managed_rendering_round_trips_and_detects_adoption() {
        let skill = doc("demo", "always", "explicit");
        let rendered = render_managed(&skill);
        let managed = inspect_managed(rendered.as_bytes()).unwrap();
        assert_eq!(managed.unmarked, render_document(&skill));
        assert_eq!(managed.marker.recorded_hash, document_hash(&skill));
        assert_eq!(managed.marker.version, SHIPPING_VERSION);
        assert!(!managed.is_user_modified());

        let edited = rendered.replace("# demo", "# user edit");
        assert!(
            inspect_managed(edited.as_bytes())
                .unwrap()
                .is_user_modified()
        );
        assert!(inspect_managed(b"plain user file").is_none());
        assert!(inspect_managed(rendered.replace("thegn_hash:", "hash:").as_bytes()).is_none());
    }

    #[test]
    fn planner_distinguishes_absent_current_upgrade_and_is_idempotent() {
        let skill = doc("demo", "always", "explicit");
        let current_registry = registry(vec![skill.clone()]);
        let absent = plan_seed(&current_registry, &target(&[]), &[], GateState::default());
        assert_eq!(absent.writes[0].reason, WriteReason::Absent);

        let applied = ExistingFile::new("demo/SKILL.md", absent.writes[0].contents.clone());
        let current = plan_seed(
            &current_registry,
            &target(&[]),
            &[applied.clone()],
            GateState::default(),
        );
        assert!(current.is_empty());
        assert_eq!(current.unchanged.len(), 1);

        let old_skill = doc("demo", "always", "explicit");
        let mut changed = skill;
        changed.body.push_str("upgrade\n");
        let upgraded_registry = registry(vec![changed]);
        let old = ExistingFile::new("demo/SKILL.md", render_managed(&old_skill));
        let upgrade = plan_seed(
            &upgraded_registry,
            &target(&[]),
            &[old],
            GateState::default(),
        );
        assert_eq!(upgrade.writes[0].reason, WriteReason::ChangedManaged);
        let applied_upgrade =
            ExistingFile::new("demo/SKILL.md", upgrade.writes[0].contents.clone());
        assert!(
            plan_seed(
                &upgraded_registry,
                &target(&[]),
                &[applied_upgrade],
                GateState::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn planner_preserves_unmarked_and_hash_mismatched_files() {
        let skill = doc("demo", "always", "explicit");
        let registry = registry(vec![skill.clone()]);
        let user = ExistingFile::new("demo/SKILL.md", b"user-owned".to_vec());
        let plan = plan_seed(&registry, &target(&[]), &[user], GateState::default());
        assert!(plan.is_empty());
        assert_eq!(plan.skipped_unmarked.len(), 1);

        let edited = render_managed(&skill).replace("# demo", "# adopted");
        let adopted = ExistingFile::new("demo/SKILL.md", edited);
        let plan = plan_seed(&registry, &target(&[]), &[adopted], GateState::default());
        assert!(plan.is_empty());
        assert_eq!(plan.skipped_adopted.len(), 1);
    }

    #[test]
    fn exclusion_and_deprecation_remove_only_intact_managed_files() {
        let keep = doc("keep", "always", "explicit");
        let excluded = doc("excluded", "always", "explicit");
        let old = doc("old", "always", "explicit");
        let registry = registry(vec![keep, excluded.clone()]);
        let existing = vec![
            ExistingFile::new("excluded/SKILL.md", render_managed(&excluded)),
            ExistingFile::new("old/SKILL.md", render_managed(&old)),
            ExistingFile::new("user/SKILL.md", b"mine".to_vec()),
        ];
        let plan = plan_seed(
            &registry,
            &target(&["excluded"]),
            &existing,
            GateState::default(),
        );
        assert_eq!(plan.removed_managed.len(), 2);
        assert_eq!(plan.removed_managed[0].reason, RemoveReason::Excluded);
        assert_eq!(plan.removed_managed[1].reason, RemoveReason::Deprecated);
        assert_eq!(plan.skipped_unmarked.len(), 1);

        let adopted_old = ExistingFile::new(
            "old/SKILL.md",
            render_managed(&old).replace("# old", "# adopted old"),
        );
        let plan = plan_seed(
            &registry,
            &target(&[]),
            &[adopted_old],
            GateState::default(),
        );
        assert!(plan.removed_managed.is_empty());
        assert_eq!(plan.skipped_adopted.len(), 1);
    }
}
