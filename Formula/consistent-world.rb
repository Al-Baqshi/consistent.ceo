# Homebrew formula for ConsistentWorld. Points at GitHub release artifacts.
# Install: brew tap Al-Baqshi/consistent.ceo https://github.com/Al-Baqshi/consistent.ceo
#          brew install consistent-world
# The sha256 values are placeholders until the first tagged release.

class ConsistentWorld < Formula
  desc "Self-hosted AI agent platform"
  homepage "https://github.com/Al-Baqshi/consistent.ceo"
  version "0.1.0"

  on_macos do
    on_intel do
      url "https://github.com/Al-Baqshi/consistent.ceo/releases/download/v0.1.0/consistent-world-x86_64-apple-darwin.tar.gz"
      sha256 "placeholder-replace-with-actual-sha-after-first-release"
    end
    on_arm do
      url "https://github.com/Al-Baqshi/consistent.ceo/releases/download/v0.1.0/consistent-world-aarch64-apple-darwin.tar.gz"
      sha256 "placeholder-replace-with-actual-sha-after-first-release"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/Al-Baqshi/consistent.ceo/releases/download/v0.1.0/consistent-world-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "placeholder-replace-with-actual-sha-after-first-release"
    end
    on_arm do
      url "https://github.com/Al-Baqshi/consistent.ceo/releases/download/v0.1.0/consistent-world-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "placeholder-replace-with-actual-sha-after-first-release"
    end
  end

  def install
    bin.install "consistent-world"
  end

  test do
    assert_match "ConsistentWorld", shell_output("#{bin}/consistent-world --help", 1)
  end
end
