# Homebrew formula template for Rewind.
#
# This is a starting point for a tap (e.g. `homebrew-rewind`). After a release,
# fill in the version and the SHA-256 sums printed by the release workflow
# (`*.tar.gz.sha256`). Until binaries are published you can also install from
# source with the `head` block or `cargo install rewind`.
class Rewind < Formula
  desc "Time-travel debugging for any local Git repository, in your terminal"
  homepage "https://github.com/rewind-dev/rewind"
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/rewind-dev/rewind/releases/download/v#{version}/rewind-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_APPLE_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/rewind-dev/rewind/releases/download/v#{version}/rewind-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_APPLE_DARWIN_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/rewind-dev/rewind/releases/download/v#{version}/rewind-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_LINUX_SHA256"
    end
    on_intel do
      url "https://github.com/rewind-dev/rewind/releases/download/v#{version}/rewind-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_X86_64_LINUX_SHA256"
    end
  end

  def install
    bin.install "rewind"
    man1.install "rewind.1"
    bash_completion.install "completions/rewind.bash" => "rewind"
    zsh_completion.install "completions/rewind.zsh" => "_rewind"
    fish_completion.install "completions/rewind.fish"
  end

  test do
    assert_match "rewind", shell_output("#{bin}/rewind --version")
    # `doctor` works even outside a Git repo and exits cleanly.
    system bin/"rewind", "doctor"
  end
end
