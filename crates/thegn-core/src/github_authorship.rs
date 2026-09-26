//! One account-bound GraphQL read. Never combine viewer and author obtained
//! through independently selected ladder layers or ambient default hosts.

use super::{GhError, gh_out};
use crate::forge::authorship::{GithubRepository, PrAuthorship, QUERY, parse_response};
use crate::remote::GitLoc;

fn request_args(repository: &GithubRepository, number: u64) -> Vec<String> {
    vec![
        "api".into(),
        "graphql".into(),
        "--hostname".into(),
        repository.host.clone(),
        "-f".into(),
        format!("query={QUERY}"),
        "-f".into(),
        format!("owner={}", repository.owner),
        "-f".into(),
        format!("name={}", repository.name),
        "-F".into(),
        format!("number={number}"),
    ]
}

pub(super) fn fetch(loc: &GitLoc, provider: &str, number: u64) -> Result<PrAuthorship, GhError> {
    let origin = loc
        .git_out(&["remote", "get-url", "origin"])
        .ok_or(GhError::NotConfigured("PR origin unavailable".into()))?;
    let repository = GithubRepository::from_origin(&origin).ok_or(GhError::NotConfigured(
        "PR origin has an unsupported authority form".into(),
    ))?;
    if number == 0
        || number > i32::MAX as u64
        || (provider == "github" && repository.host != "github.com")
        || (provider == "ghe" && repository.host == "github.com")
    {
        return Err(GhError::NotConfigured(
            "PR authority does not match the selected forge".into(),
        ));
    }
    let args = request_args(&repository, number);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let response = gh_out(loc, &refs)?;
    let value = serde_json::from_str(&response)
        .map_err(|_| GhError::Other("invalid PR authorship response".into()))?;
    parse_response(&value, provider, repository, number).ok_or_else(|| {
        GhError::Other("PR authorship identity is incomplete or inconsistent".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_pins_host_and_repository_and_fetches_viewer_with_author() {
        let repo = GithubRepository::from_origin("git@forge.example:org/repo.git").unwrap();
        let args = request_args(&repo, 17);
        assert_eq!(
            &args[..4],
            ["api", "graphql", "--hostname", "forge.example"]
        );
        assert!(args.iter().any(|arg| arg == "owner=org"));
        assert!(args.iter().any(|arg| arg == "name=repo"));
        assert!(args.iter().any(|arg| arg == "number=17"));
        let query = args.iter().find(|arg| arg.starts_with("query=")).unwrap();
        assert!(query.contains("viewer{__typename id login}"));
        assert!(query.contains("author{__typename login ... on User{id}}"));
    }
}
