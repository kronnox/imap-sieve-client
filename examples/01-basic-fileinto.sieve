# Basic fileinto: move spam to Junk, keep everything else.
require "fileinto";

if header :contains "Subject" "spam" {
    fileinto "Junk";
}

# Implicit keep — messages not matching any rule stay in INBOX.