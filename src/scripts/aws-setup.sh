#!/usr/bin/env bash
# Creates the AWS IAM user that Listaculous uses for automatic SES setup — no AWS console needed.
#
# Requires the AWS CLI (v2), signed in as someone allowed to manage IAM
# (check with: aws sts get-caller-identity).
#
# Optional environment variables:
#   LISTACULOUS_API_KEY=lc_…      connect the new key to Listaculous automatically
#   LISTACULOUS_SETUP_USER=name   IAM user name (default: listaculous-setup)
#   LISTACULOUS_SES_REGION=region default SES region for setup (default: __REGION__)
#   AWS_PROFILE=…                 which AWS CLI profile to use
set -euo pipefail

LISTACULOUS_URL="__URL__"
USER_NAME="${LISTACULOUS_SETUP_USER:-listaculous-setup}"
REGION="${LISTACULOUS_SES_REGION:-__REGION__}"

command -v aws >/dev/null || { echo "The AWS CLI isn't installed: https://aws.amazon.com/cli/" >&2; exit 1; }
ACCOUNT=$(aws sts get-caller-identity --query Account --output text) \
  || { echo "The AWS CLI isn't signed in. Run 'aws configure' or 'aws sso login' first." >&2; exit 1; }
echo "AWS account: $ACCOUNT"

if aws iam get-user --user-name "$USER_NAME" >/dev/null 2>&1; then
  echo "IAM user $USER_NAME already exists; updating its policy."
else
  aws iam create-user --user-name "$USER_NAME" --tags Key=managed-by,Value=listaculous >/dev/null
  echo "Created IAM user $USER_NAME"
fi

POLICY_FILE=$(mktemp)
KEY_ID=""
SECRET=""
HANDED_OFF=0
print_credentials() {
  cat <<OUT

Paste these into Listaculous → Integrations → Connect an AWS account
(the secret is shown only once):

  Access key ID:      $KEY_ID
  Secret access key:  $SECRET
  Region:             $REGION

Once your sites are set up you can delete this key:
  aws iam delete-access-key --user-name $USER_NAME --access-key-id $KEY_ID
OUT
}
# Whatever happens after the key exists (error, Ctrl-C), never lose its secret: print it unless
# Listaculous confirmed it stored the key, or it was already printed.
on_exit() {
  # Ignore further Ctrl-C / TERM while cleaning up, so a second signal can't cut off the printout.
  trap '' INT TERM
  rm -f "$POLICY_FILE"
  if [ -n "$KEY_ID" ] && [ "$HANDED_OFF" != 1 ]; then print_credentials; fi
}
trap on_exit EXIT
trap 'exit 130' INT TERM
cat > "$POLICY_FILE" <<'POLICY'
__POLICY__
POLICY
aws iam put-user-policy --user-name "$USER_NAME" --policy-name listaculous-setup --policy-document "file://$POLICY_FILE"
echo "Attached the Listaculous setup policy"

KEYS=$(aws iam list-access-keys --user-name "$USER_NAME" --query 'length(AccessKeyMetadata)' --output text)
if [ "$KEYS" -ge 2 ]; then
  echo "$USER_NAME already has 2 access keys (the AWS maximum). Delete one, then re-run:" >&2
  echo "  aws iam list-access-keys --user-name $USER_NAME" >&2
  echo "  aws iam delete-access-key --user-name $USER_NAME --access-key-id <AccessKeyId>" >&2
  exit 1
fi
read -r KEY_ID SECRET < <(aws iam create-access-key --user-name "$USER_NAME" \
  --query 'AccessKey.[AccessKeyId,SecretAccessKey]' --output text)
echo "Created access key $KEY_ID"

if [ -n "${LISTACULOUS_API_KEY:-}" ]; then
  # Key characters are [A-Za-z0-9/+], so they're safe to place in JSON as-is.
  BODY=$(printf '{"kind":"aws","config":{"access_key_id":"%s","secret_access_key":"%s","region":"%s"}}' "$KEY_ID" "$SECRET" "$REGION")
  # New IAM keys can take a few seconds to start working; Listaculous verifies the key, so retry.
  # Success means exactly HTTP 201 with the created AWS integration (a redirect, say, is not).
  for _ in 1 2 3 4 5 6; do
    RESP_FILE=$(mktemp)
    # Bounded, so an unresponsive server can't hang us before the key is printed.
    CODE=$(curl -sS --connect-timeout 10 --max-time 45 -o "$RESP_FILE" -w '%{http_code}' -X POST "$LISTACULOUS_URL/api/integrations" \
        -H "Authorization: Bearer $LISTACULOUS_API_KEY" -H "Content-Type: application/json" \
        --data-binary @- <<<"$BODY" 2>/dev/null) || CODE=000
    RESP=$(cat "$RESP_FILE"); rm -f "$RESP_FILE"
    if [ "$CODE" = 201 ] && grep -q '"kind":"aws"' <<<"$RESP"; then
      HANDED_OFF=1
      echo "Connected to Listaculous ($LISTACULOUS_URL). You can now run Automatic setup for a site."
      exit 0
    fi
    # The setup key is single-use: if it's gone (used/revoked/expired) retrying can't help.
    case "$CODE" in 401|403|409) break ;; esac
    sleep 5
  done
  echo "Couldn't connect automatically (HTTP $CODE): $RESP" >&2
  echo "Paste the key below into Listaculous → Integrations → Connect an AWS account." >&2
fi

print_credentials
HANDED_OFF=1
