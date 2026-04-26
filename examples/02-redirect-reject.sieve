# Redirect and reject actions.
# Note: requires [smtp] section in config.toml.

require "reject";

# Redirect all mail from a specific sender to another address.
if address :is "From" "newsletter@example.com" {
    redirect "archive@example.com";
}

# Reject mail with certain subject patterns.
if header :contains "Subject" "viagra" {
    reject "Message rejected: spam content detected.";
}

# Implicit keep for everything else.