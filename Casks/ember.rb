cask "ember" do
  version "0.2.0"

  on_arm do
    sha256 "0decb12623db79f7fd7ed483b14d7fdea06dfc801decbc5f2549881eac16d5a8"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-macos-arm64.zip",
        verified: "github.com/kingb/ember/"
  end
  on_intel do
    sha256 "1a210a2be3dda64dd64bac9f224da6c7e0a92bd6b24037a53899b88e3ef77fc8"

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
