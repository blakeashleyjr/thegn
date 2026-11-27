#!/usr/bin/env bash
# Authenticated downloads shared by the repository's bootstrap scripts
# (scripts/ci/ageless-runner-setup.sh and setup-macos.sh).
#
# The contract is authenticate-then-execute. Callers pin the exact byte size and
# SHA-256 of a released artifact in reviewed repository data, download it into a
# private staging directory under hard timeouts and a byte cap, verify the bytes,
# and only then run or install them. Nothing here treats an existing file as
# trusted: a cache hit is re-authenticated on every use, because existence is not
# integrity. TLS authenticates transport, not the artifact, so the pinned digest
# — not the host or the redirect target — decides whether bytes may execute.

# Report a refusal through the caller's `die` when it has one, so bootstrap
# operators see one consistent message format.
bootstrap_refuse() {
  local message="$1"
  if declare -F die >/dev/null 2>&1; then
    die "$message"
  else
    printf '%s\n' "$message" >&2
  fi
  return 1
}

# Print the lowercase SHA-256 of a file. Fails when neither hashing tool exists,
# rather than silently accepting unauthenticated bytes.
bootstrap_sha256_file() {
  local path="$1" digest
  if command -v sha256sum >/dev/null 2>&1; then
    digest="$(sha256sum "$path")" || return 1
  elif command -v shasum >/dev/null 2>&1; then
    digest="$(shasum -a 256 "$path")" || return 1
  else
    return 1
  fi
  printf '%s\n' "${digest%% *}"
}

# A reviewed pin is a lowercase 64-hex digest, a positive byte size, and a
# positive cap that the size fits inside. Anything else is a data error, not a
# licence to download unverified bytes.
bootstrap_valid_pin() {
  local sha256="$1" size="$2" max_bytes="$3"
  [[ $sha256 =~ ^[a-f0-9]{64}$ ]] || return 1
  [[ $size =~ ^[1-9][0-9]*$ ]] || return 1
  [[ $max_bytes =~ ^[1-9][0-9]*$ ]] || return 1
  ((size <= max_bytes))
}

# A fresh, unguessable, caller-private directory for one download.
bootstrap_staging_dir() {
  local dir
  dir="$(mktemp -d "${TMPDIR:-/tmp}/thegn-bootstrap.XXXXXXXX")" || return 1
  if ! chmod 0700 "$dir"; then
    rm -rf -- "$dir"
    return 1
  fi
  printf '%s\n' "$dir"
}

# Authenticate already-local bytes: regular file (never a symlink), exact size,
# exact digest. This is what re-checks a cached artifact on every use.
bootstrap_verify_bytes() {
  local path="$1" expected_sha256="$2" expected_size="$3"
  local actual_size actual_sha256
  if [ -L "$path" ] || [ ! -f "$path" ]; then
    bootstrap_refuse "staged artifact is missing or a symlink: $path"
    return 1
  fi
  actual_size="$(wc -c <"$path")" || return 1
  actual_size="${actual_size//[[:space:]]/}"
  if [ "$actual_size" != "$expected_size" ]; then
    bootstrap_refuse "pinned artifact size mismatch: expected $expected_size bytes, got ${actual_size:-unknown}"
    return 1
  fi
  if ! actual_sha256="$(bootstrap_sha256_file "$path")"; then
    bootstrap_refuse 'sha256sum or shasum is required to authenticate an artifact'
    return 1
  fi
  if [ "$actual_sha256" != "$expected_sha256" ]; then
    bootstrap_refuse "pinned artifact SHA-256 mismatch: expected $expected_sha256, got $actual_sha256"
    return 1
  fi
  return 0
}

# Download one pinned artifact into a caller-supplied staging path and verify it.
#
#   $1 url          https URL of the pinned release asset
#   $2 sha256       reviewed digest
#   $3 size         reviewed exact byte size
#   $4 max_bytes    hard byte cap for any body (including a redirect's)
#   $5 max_seconds  total transfer budget
#   $6 destination  path inside the caller's private staging directory
#   $7 label        human name used in refusals
#   $8 hosts        space-separated approved origin hosts for the pinned URL
#                   and every redirect hop's final origin
bootstrap_download_pinned() {
  local url="$1" expected_sha256="$2" expected_size="$3" max_bytes="$4"
  local max_seconds="$5" destination="$6" label="$7" approved_hosts="$8"
  local effective authority host host_lower candidate approved
  if ! bootstrap_valid_pin "$expected_sha256" "$expected_size" "$max_bytes"; then
    bootstrap_refuse "invalid pinned identity for the $label"
    return 1
  fi
  case "$url" in
  https://*) ;;
  *)
    bootstrap_refuse "pinned $label URL must be https: $url"
    return 1
    ;;
  esac
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    bootstrap_refuse "staging path already exists for the $label: $destination"
    return 1
  fi
  # --connect-timeout bounds the handshake, --speed-limit/--speed-time abort a
  # stalled body, --max-time bounds the whole transfer, --max-filesize caps the
  # body, and --proto-redir refuses any non-https hop. The final origin must be
  # on the reviewed list, so a redirect cannot silently move the transfer to an
  # unapproved host; the pinned digest still decides whether the bytes execute.
  if ! effective="$(curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --connect-timeout 30 --speed-limit 1024 --speed-time 60 \
    --max-time "$max_seconds" --max-filesize "$max_bytes" \
    --write-out '%{url_effective}' -o "$destination" "$url")"; then
    bootstrap_refuse "could not download the pinned $label from $url"
    return 1
  fi
  case "$effective" in
  https://*) ;;
  *)
    bootstrap_refuse "the pinned $label resolved to a non-https origin: ${effective:-unknown}"
    return 1
    ;;
  esac
  authority="${effective#https://}"
  authority="${authority%%/*}"
  case "$authority" in
  '' | *@*)
    bootstrap_refuse "the pinned $label resolved to an unusable origin: $effective"
    return 1
    ;;
  esac
  host="${authority%%:*}"
  host_lower="$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')"
  approved=''
  for candidate in $approved_hosts; do
    if [ "$host_lower" = "$candidate" ]; then
      approved=1
      break
    fi
  done
  if [ -z "$approved" ]; then
    bootstrap_refuse "the pinned $label resolved to an unapproved origin '$host_lower' (approved: ${approved_hosts:-none})"
    return 1
  fi
  if ! bootstrap_verify_bytes "$destination" "$expected_sha256" "$expected_size"; then
    return 1
  fi
  chmod 0600 "$destination" || return 1
}
