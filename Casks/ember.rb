cask "ember" do
  version "0.5.0"

  on_arm do
    sha256 "49400c9b846c9b7be152a635ac7c089e145530a1d680896a42d1707e238d0279"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-macos-arm64.zip",
        verified: "github.com/kingb/ember/"
  end
  on_intel do
    sha256 "6872ef37bded46d022ca9ad29b049354817bae0749bf51f2d57aeefc8952f89f"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-macos-x86_64.zip",
        verified: "github.com/kingb/ember/"
  end

  name "Ember"
  desc "GPU-accelerated campfire terminal emulator"
  homepage "https://emberterm.com/"

  # Auto-detect new versions from GitHub releases (brew livecheck / bump).
  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: :big_sur

  app "Ember.app"

  # Remove user config on `brew uninstall --zap`.
  zap trash: "~/.config/ember"
end
