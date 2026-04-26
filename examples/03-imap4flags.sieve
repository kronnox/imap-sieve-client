# IMAP flag manipulation with imap4flags and fileinto.
require "fileinto";
require "imap4flags";

# Mark messages from the boss as flagged and important.
if address :contains "From" "boss@company.com" {
    addflag "\\Flagged";
    fileinto "Important";
}

# Mark newsletter messages as already read and file them.
if header :contains "List-Id" "newsletter" {
    addflag "\\Seen";
    fileinto "Newsletters";
}

# Remove Seen flag from messages that need attention.
if header :contains "X-Priority" "1" {
    removeflag "\\Seen";
}

# Implicit keep for everything else.