cask "ember" do
  version "0.1.0"
  sha256 "1943910b367153150d22dd0caaa269ac8730e186c2dec2891f20721f6b0f94f1"

  url "https://github.com/kingb/ember-term/releases/download/v#{version}/Ember-#{version}.zip",
      verified: "github.com/kingb/ember-term/"
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

  caveats <<~EOS
    Ember is currently ad-hoc signed, not notarized, so on first launch macOS
    Gatekeeper will warn. Either:
      • right-click Ember.app → Open (once), or
      • run: xattr -dr com.apple.quarantine "#{appdir}/Ember.app"
    A notarized build will remove this step.
  EOS
end
