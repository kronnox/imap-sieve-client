# Realistic multi-rule sieve script combining several extensions.
require "fileinto";
require "copy";
require "mailbox";
require "imap4flags";

# Rule 1: Spam filtering — move to Junk.
if header :contains "X-Spam-Flag" "YES" {
    fileinto "Junk";
}

# Rule 2: Mailing list sorting — create mailbox if needed.
if header :matches "List-Id" "<*>" {
    addflag "\\Seen";
    fileinto :create "Lists";
}

# Rule 3: Important sender — flag and keep a copy in Archive.
if address :is "From" "ceo@company.com" {
    addflag "\\Flagged";
    fileinto :copy "Archive";
}

# Rule 4: Large messages — file into AttachmentReview.
if size :over 1M {
    fileinto "AttachmentReview";
}

# Implicit keep for everything else.