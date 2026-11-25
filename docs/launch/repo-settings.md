# Repo settings for the public launch

THE-2619 asks for launch hygiene that GitHub keeps outside the git tree:
the repository description and topics, the social preview image, the
Discussions categories, and a pinned "start here" discussion. This page is
the owner runbook for those: each item is applied once, by the repository
owner, with `gh` or the GitHub UI, before the launch announcement. Nothing
here runs automatically.

The in-repo halves of the issue are files, tracked by git:

- Issue templates — `.github/ISSUE_TEMPLATE/` (bug report, feature request,
  contact links).
- PR template — `.github/pull_request_template.md`.
- Discussion templates — `.github/DISCUSSION_TEMPLATE/` (Q&A, idea,
  show and tell).
- Community pages — `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`,
  `SUPPORT.md`.

## Description

Proposed text, exactly (the README's own tagline, so the repository page
and the README agree on launch day):

```text
A terminal-native Git worktree IDE that is its own multiplexer.
```

Apply:

```sh
gh repo edit blakeashleyjr/thegn \
  --description "A terminal-native Git worktree IDE that is its own multiplexer."
```

## Topics

GitHub topics are lowercase, hyphen-separated, and capped at twenty per
repository. Proposed list, exactly (twelve):

```text
git
git-worktrees
git-client
terminal
terminal-multiplexer
tui
rust
rust-lang
ide
cli
developer-tools
developer-experience
```

Apply with repeated `--add-topic` (additive; it never removes an existing
topic):

```sh
gh repo edit blakeashleyjr/thegn \
  --add-topic git --add-topic git-worktrees --add-topic git-client \
  --add-topic terminal --add-topic terminal-multiplexer --add-topic tui \
  --add-topic rust --add-topic rust-lang --add-topic ide --add-topic cli \
  --add-topic developer-tools --add-topic developer-experience
```

Review the final set (and remove anything stale with `--remove-topic`) with:

```sh
gh repo view blakeashleyjr/thegn --json repositoryTopics \
  --jq '.repositoryTopics[].name'
```

## Social preview

The issue names no source asset, so no image file is added to the tree —
the same rule the README already states for the demo GIF: no asset exists
yet, do not add a link that would 404. The owner composes and uploads the
card before the announcement:

1. Target a 1280×640 px (2:1) PNG or JPG; GitHub's upload dialog asks for
   a file under 1 MB.
2. Candidate sources already in the tree are the 800×480 images under
   `docs/assets/` (`thegn-prism.png` and the three theme screenshots);
   review them, recompose one onto a 1280×640 canvas, or commission a
   fresh card.
3. Upload under Settings → General → Social preview (Edit → Upload an
   image).

## Discussions

Enable Discussions first (Settings → General → Features → Discussions).
GitHub's default category set already covers the proposed list; delete
Polls to match it exactly, or keep Polls — nothing here depends on it:

| Category      | Purpose                                       | Template                  |
| ------------- | --------------------------------------------- | ------------------------- |
| Announcements | Releases and news; maintainers only.          | none (posts are authored) |
| General       | Open discussion.                              | none (free-form)          |
| Q&A           | Questions with a marked answer.               | `q_and_a.yml`             |
| Ideas         | Proposals to shape before they become issues. | `ideas.yml`               |
| Show and tell | Setups, themes and workflows.                 | `show_and_tell.yml`       |

GitHub offers every template file in the new-discussion picker regardless
of category, so the pairings above are convention, kept honest by the
templates' own wording, not enforced by GitHub.

## "Start here" pinned discussion

Post the body in the Announcements category, then pin it. Exact title:

```text
Start here — welcome to thegn
```

The exact body is
[`docs/launch/start-here-discussion.md`](start-here-discussion.md);
paste it whole. The UI path: Discussions → New discussion → Announcements,
paste, publish, then Pin from the discussion's menu. The API path:

```sh
REPO_ID="$(gh api repos/blakeashleyjr/thegn --jq .node_id)"

CATEGORY_ID="$(gh api graphql \
  -f owner=blakeashleyjr -f name=thegn \
  -f query='query($owner: String!, $name: String!) {
    repository(owner: $owner, name: $name) {
      discussionCategories(first: 20) { nodes { id name } }
    }
  }' \
  --jq '.data.repository.discussionCategories.nodes[]
    | select(.name == "Announcements")
    | .id')"

DISCUSSION_ID="$(gh api graphql \
  -f query='mutation($repositoryId: ID!, $categoryId: ID!, $title: String!, $body: String!) {
    createDiscussion(input: {
      repositoryId: $repositoryId
      categoryId: $categoryId
      title: $title
      body: $body
    }) { discussion { id } }
  }' \
  -f repositoryId="$REPO_ID" -f categoryId="$CATEGORY_ID" \
  -f title='Start here — welcome to thegn' \
  -f body="$(cat docs/launch/start-here-discussion.md)" \
  --jq '.data.createDiscussion.discussion.id')"

gh api graphql \
  -f query='mutation($discussionId: ID!) {
    pinDiscussion(input: { discussionId: $discussionId }) { discussion { url } }
  }' \
  -f discussionId="$DISCUSSION_ID"
```

The body quotes no version numbers, so it does not rot with releases.

## Launch-day order

1. Merge the in-repo half of THE-2619 (the templates and this runbook) so
   it is live on `main`.
2. Enable Discussions and check the categories against the table above.
3. Post and pin the start-here discussion.
4. Set the description and the topics.
5. Upload the social preview.
6. Verify signed out: the repository page, a link unfurl (share the repo
   URL in a private message and read the card), and both template pickers
   (new issue, new discussion).
