cask "ember" do
  version "0.1.0"

  on_arm do
    sha256 "7725538b70de1333bf15f81a6b3da2c2eb553262b8300be8559bd50a4dd78ddc"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}.zip",
        verified: "github.com/kingb/ember/"
  end
  on_intel do
    sha256 "2b3f8e9922e2fd0748e583e25bb073d97eed59c90cdec9a7399a8629291e076d"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-x86_64.zip",
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
