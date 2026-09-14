//! Fresh, provider-authenticated PR ownership evidence. Persisted PR data is
//! descriptive only; authorization also requires the viewer from this request.

use super::model::{PrAuthor, PrStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepository {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl GithubRepository {
    /// Deliberately limited to unambiguous GitHub HTTPS/SSH remote forms.
    /// Unsupported ports, userinfo, query/fragment and local paths
    /// fail closed rather than guessing the credential authority.
    pub fn from_origin(origin: &str) -> Option<Self> {
        if origin.trim() != origin
            || !origin.is_ascii()
            || origin.bytes().any(|b| b.is_ascii_control())
        {
            return None;
        }
        let (host, path) = if let Some(rest) = origin.strip_prefix("https://") {
            rest.split_once('/')?
        } else if let Some(rest) = origin.strip_prefix("ssh://git@") {
            rest.split_once('/')?
        } else if let Some(rest) = origin.strip_prefix("git@") {
            rest.split_once(':')?
        } else {
            return None;
        };
        if host.is_empty()
            || !host.split('.').all(|part| {
                !part.is_empty()
                    && !part.starts_with('-')
                    && !part.ends_with('-')
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return None;
        }
        let (owner, name) = path.strip_suffix(".git").unwrap_or(path).split_once('/')?;
        let component = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.len() <= 256
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        };
        if !component(owner) || !component(name) {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            owner: owner.into(),
            name: name.into(),
        })
    }

    pub fn matches_pr_url(&self, url: &str, number: u64) -> bool {
        let expected = format!(
            "https://{}/{}/{}/pull/{number}",
            self.host, self.owner, self.name
        );
        expected.eq_ignore_ascii_case(url)
    }
}

/// Intentionally not serde: a cached/deserialized value cannot become fresh
/// evidence. One provider request supplies every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrAuthorship {
    pub provider: String,
    pub repository: GithubRepository,
    pub repository_id: String,
    pub pr_id: String,
    pub number: u64,
    pub head: String,
    pub state: String,
    pub author: PrAuthor,
    pub viewer: PrAuthor,
}

fn canonical_identity(identity: &PrAuthor) -> bool {
    !identity.is_bot
        && !identity.id.is_empty()
        && identity.id.len() <= 512
        && identity
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'='))
        && !identity.login.is_empty()
        && identity.login.len() <= 256
        && identity
            .login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'='))
}

impl PrAuthorship {
    pub fn authorizes(&self, provider: &str, repository: &GithubRepository, pr: &PrStatus) -> bool {
        let Some(author) = &pr.author else {
            return false;
        };
        matches!(provider, "github" | "ghe")
            && self.provider == provider
            && &self.repository == repository
            && (provider != "github" || repository.host == "github.com")
            && (provider != "ghe" || repository.host != "github.com")
            && node_id(&self.repository_id)
            && node_id(&self.pr_id)
            && self.number > 0
            && self.number == pr.number
            && repository.matches_pr_url(&pr.url, self.number)
            && self.state == "OPEN"
            && pr.state == "OPEN"
            && matches!(self.head.len(), 40 | 64)
            && self.head.bytes().all(|b| b.is_ascii_hexdigit())
            && self.head == pr.head_ref_oid
            && canonical_identity(author)
            && canonical_identity(&self.author)
            && canonical_identity(&self.viewer)
            && author.id == self.author.id
            && self.author.id == self.viewer.id
    }

    /// Login renames retain a stable node identity. A changed account, PR or
    /// repository is a new authority context even when display names match.
    pub fn same_context(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.repository == other.repository
            && self.repository_id == other.repository_id
            && self.pr_id == other.pr_id
            && self.number == other.number
            && self.author.id == other.author.id
            && self.viewer.id == other.viewer.id
    }
}

pub const QUERY: &str = r#"query($owner:String!,$name:String!,$number:Int!){
 viewer{__typename id login}
 repository(owner:$owner,name:$name){id nameWithOwner pullRequest(number:$number){
  id number url state headRefOid author{__typename login ... on User{id}}
 }}
}"#;

/// Strict full-envelope parsing: GraphQL partial success is not authority.
pub fn parse_response(
    value: &serde_json::Value,
    provider: &str,
    repository: GithubRepository,
    number: u64,
) -> Option<PrAuthorship> {
    if value
        .get("errors")
        .is_some_and(|e| !e.as_array().is_some_and(Vec::is_empty))
    {
        return None;
    }
    let data = value.get("data")?;
    let repo = data.get("repository")?;
    let pr = repo.get("pullRequest")?;
    if !repo
        .get("nameWithOwner")?
        .as_str()?
        .eq_ignore_ascii_case(&format!("{}/{}", repository.owner, repository.name))
        || pr.get("number")?.as_u64()? != number
        || !repository.matches_pr_url(pr.get("url")?.as_str()?, number)
    {
        return None;
    }
    if pr.get("author")?.get("__typename")?.as_str()? != "User" {
        return None;
    }
    let identity = |v: &serde_json::Value| -> Option<PrAuthor> {
        if v.get("__typename")?.as_str()? != "User" {
            return None;
        }
        Some(PrAuthor {
            id: v.get("id")?.as_str()?.into(),
            login: v.get("login")?.as_str()?.into(),
            is_bot: false,
        })
    };
    let proof = PrAuthorship {
        provider: provider.into(),
        repository,
        repository_id: repo.get("id")?.as_str()?.into(),
        pr_id: pr.get("id")?.as_str()?.into(),
        number,
        head: pr.get("headRefOid")?.as_str()?.into(),
        state: pr.get("state")?.as_str()?.into(),
        author: identity(pr.get("author")?)?,
        viewer: identity(data.get("viewer")?)?,
    };
    if !canonical_identity(&proof.author)
        || !canonical_identity(&proof.viewer)
        || !node_id(&proof.repository_id)
        || !node_id(&proof.pr_id)
    {
        return None;
    }
    Some(proof)
}

#[cfg(test)]
#[path = "authorship_tests.rs"]
mod tests;
