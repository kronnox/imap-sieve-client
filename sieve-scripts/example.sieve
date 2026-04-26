require "fileinto";
if header :contains "Subject" "spam" {
    fileinto "Junk";
}