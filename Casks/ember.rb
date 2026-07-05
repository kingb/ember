cask "ember" do
  version "0.1.0"
  sha256 "7725538b70de1333bf15f81a6b3da2c2eb553262b8300be8559bd50a4dd78ddc"

  url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}.zip",
      verified: "github.com/kingb/ember/"
  name "Ember"
  desc "GPU-accelerated campfire terminal emulator"
  homepage "https://emberterm.com"

  # Auto-detect new versions from GitHub releases (brew livecheck / bump).
  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :big_sur"

  app "Ember.app"

  # Remove user config on `brew uninstall --zap`.
  zap trash: "~/.config/ember"
end
