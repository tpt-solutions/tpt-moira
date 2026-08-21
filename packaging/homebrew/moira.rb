# Homebrew formula template for Moira.
#
# This is a packaging template, not a final published formula. Maintainers fill
# in the real version, URL, and SHA-256 (produced by `x.py dist` as
# `dist/tpt-moira-<version>-checksums.txt`) before submitting to a tap.
#
# Usage (once published):
#   brew install tptsolutions/tap/moira
#
# See docs/STABILITY.md for the versioning / release policy this formula follows.
class Moira < Formula
  desc "High-performance, from-scratch Rust compiler by TPT Solutions"
  homepage "https://github.com/tptsolutions/moira"
  # url "https://github.com/tptsolutions/moira/releases/download/v0.1.0/moira-0.1.0-x86_64-apple-darwin.tar.gz"
  # sha256 "REPLACE_WITH_RELEASE_SHA256"
  license "MIT OR Apache-2.0"
  version "0.1.0"

  on_macos do
    on_arm do
      # url ".../moira-#{version}-aarch64-apple-darwin.tar.gz"
      # sha256 "REPLACE"
    end
    on_intel do
      # url ".../moira-#{version}-x86_64-apple-darwin.tar.gz"
      # sha256 "REPLACE"
    end
  end

  on_linux do
    on_arm do
      # url ".../moira-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      # sha256 "REPLACE"
    end
    on_intel do
      # url ".../moira-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      # sha256 "REPLACE"
    end
  end

  def install
    bin.install "bin/moira"
    doc.install "README.md" if File.exist?("README.md")
    (share/"licenses/moira").install "LICENSE-MIT" if File.exist?("LICENSE-MIT")
    (share/"licenses/moira").install "LICENSE-APACHE-2.0" if File.exist?("LICENSE-APACHE-2.0")
  end

  test do
    # Smoke test: the driver should emit its parse summary on a trivial program.
    (testpath/"hello.rs").write <<~EOS
      fn main() {
          let x = 1 + 1;
          let _ = x;
      }
    EOS
    assert_match "top-level item", shell_output("#{bin}/moira --emit ast #{testpath}/hello.rs")
  end
end
