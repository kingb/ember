cask "ember" do
  version "0.6.0"

  on_arm do
    sha256 "1ba7af58a29957b3bddf94cf8bc59f938b56f7a7f192c819e6681992021dd82b"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-macos-arm64.zip",
        verified: "github.com/kingb/ember/"
  end
  on_intel do
    sha256 "e1c465806532a0641ade01601adae1239894285e132d5ede02816ef01920f6ee"

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
