cask "ember" do
  version "0.6.1"

  on_arm do
    sha256 "47002ea51e79ea364eb8acee67aa2405c7b71810b36d5a5afa892aba1b6e1fd2"

    url "https://github.com/kingb/ember/releases/download/v#{version}/Ember-#{version}-macos-arm64.zip",
        verified: "github.com/kingb/ember/"
  end
  on_intel do
    sha256 "97c1feeb0913fd5dfc521c2d62386aa3b36f0c89cc2c7e956c53d35aacc63a3a"

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
