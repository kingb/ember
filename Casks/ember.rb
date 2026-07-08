cask "ember" do
  version "0.3.1"

  on_arm do
    sha256 "ab39318024143ed21898394105171a41ceb38afa42b2a56f8b037ec4cb3c7c01"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-macos-arm64.zip",
        verified: "github.com/kingb/ember/"
  end
  on_intel do
    sha256 "5db234772d34795bb76f886064acf0b2930a71fbb1bad8fb5d040699c896e0fe"

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
