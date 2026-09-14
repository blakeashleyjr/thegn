use super::*;
use serde_json::{Value, json};

fn repository() -> GithubRepository {
    GithubRepository::from_origin("git@github.com:organization/project.git").unwrap()
}
fn response() -> Value {
    json!({"data": {"viewer": {"__typename":"User", "id":"U_own", "login":"me"}, "repository": {
        "id":"R_project", "nameWithOwner":"organization/project", "pullRequest": {
            "id":"PR_7", "number":7, "title":"fixture", "url":"https://github.com/organization/project/pull/7",
            "state":"OPEN", "headRefOid":"a".repeat(40),
            "author":{"__typename":"User", "id":"U_own", "login":"me"}
        }
    }}})
}
fn pr(value: &Value) -> PrStatus {
    serde_json::from_value(value["data"]["repository"]["pullRequest"].clone()).unwrap()
}

#[test]
fn actual_cli_model_authors_organization_pr_and_never_guesses_namespace() {
    let value = response();
    let model = pr(&value);
    let proof = parse_response(&value, "github", repository(), 7).unwrap();
    assert!(proof.authorizes("github", &repository(), &model));
    // Someone else opened a PR in my namespace: URL ownership grants nothing.
    let mut value = response();
    value["data"]["repository"]["nameWithOwner"] = json!("me/project");
    value["data"]["repository"]["pullRequest"]["url"] =
        json!("https://github.com/me/project/pull/7");
    value["data"]["repository"]["pullRequest"]["author"]["id"] = json!("U_other");
    value["data"]["repository"]["pullRequest"]["author"]["login"] = json!("other");
    let repo = GithubRepository::from_origin("https://github.com/me/project.git").unwrap();
    assert!(
        !parse_response(&value, "github", repo.clone(), 7)
            .unwrap()
            .authorizes("github", &repo, &pr(&value))
    );
}

#[test]
fn legacy_cache_deleted_bot_empty_and_partial_authority_fail_closed() {
    let mut value = response();
    value["data"]["repository"]["pullRequest"]
        .as_object_mut()
        .unwrap()
        .remove("author");
    let cached = pr(&value);
    assert!(cached.author.is_none());
    let proof = parse_response(&response(), "github", repository(), 7).unwrap();
    assert!(!proof.authorizes("github", &repository(), &cached));
    for pointer in ["/data/viewer", "/data/repository/pullRequest/author"] {
        for bad in [
            Value::Null,
            json!({}),
            json!({"id":"", "login":"me"}),
            json!({"id":"U_x", "login":""}),
            json!({"id":2,"login":"me"}),
            json!({"__typename":"Bot", "id":"U_own", "login":"me"}),
        ] {
            let mut value = response();
            *value.pointer_mut(pointer).unwrap() = bad;
            let parsed = parse_response(&value, "github", repository(), 7);
            assert!(
                parsed.is_none_or(|proof| !proof.authorizes(
                    "github",
                    &repository(),
                    &pr(&response())
                )),
                "{pointer}"
            );
        }
    }
    for errors in [json!([{"message":"forbidden"}]), Value::Null, json!({})] {
        let mut value = response();
        value["errors"] = errors;
        assert!(parse_response(&value, "github", repository(), 7).is_none());
    }
}

#[test]
fn stable_ids_bind_account_author_head_pr_and_repository_generations() {
    let value = response();
    let model = pr(&value);
    let original = parse_response(&value, "github", repository(), 7).unwrap();
    let mut renamed = original.clone();
    renamed.viewer.login = "new-login".into();
    renamed.author.login = "new-login".into();
    assert!(renamed.authorizes("github", &repository(), &model));
    assert!(renamed.same_context(&original));
    for change in 0..8 {
        let mut changed = original.clone();
        match change {
            0 => changed.viewer.id = "U_other".into(),
            1 => changed.author.id = "U_other".into(),
            2 => changed.repository.host = "other.example".into(),
            3 => changed.repository.name = "other".into(),
            4 => changed.provider = "ghe".into(),
            5 => changed.number = 8,
            6 => changed.head = "b".repeat(40),
            _ => changed.state = "CLOSED".into(),
        }
        assert!(
            !changed.authorizes("github", &repository(), &model),
            "change {change}"
        );
    }
    for change in 0..2 {
        let mut changed = original.clone();
        if change == 0 {
            changed.repository_id = "R_replaced".into();
        } else {
            changed.pr_id = "PR_replaced".into();
        }
        assert!(!changed.same_context(&original));
    }
}

#[test]
fn enterprise_host_is_explicit_and_ambiguous_origins_are_refused() {
    for origin in [
        "https://github.com/organization/project.git",
        "git@github.com:organization/project.git",
        "ssh://git@github.com/organization/project.git",
    ] {
        assert_eq!(GithubRepository::from_origin(origin), Some(repository()));
    }
    for origin in [
        "/local/repo",
        "https://user@github.com/org/repo",
        "https://github.com:443/org/repo",
        "git@github.com:org/repo/extra",
        "https://github.com/org/repo?query",
        "https://github.com/org/repo#fragment",
        "https://github.com/../repo",
        "https://github.com/org/repo\n",
    ] {
        assert!(GithubRepository::from_origin(origin).is_none(), "{origin}");
    }
    let mut value = response();
    value["data"]["repository"]["pullRequest"]["url"] =
        json!("https://work.example/organization/project/pull/7");
    let repo = GithubRepository::from_origin("git@work.example:organization/project.git").unwrap();
    let proof = parse_response(&value, "ghe", repo.clone(), 7).unwrap();
    assert!(proof.authorizes("ghe", &repo, &pr(&value)));
    assert!(!proof.authorizes("github", &repo, &pr(&value)));
    assert!(!proof.authorizes("gitlab", &repo, &pr(&value)));
    assert!(!proof.authorizes("forgejo", &repo, &pr(&value)));
}
