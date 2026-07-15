class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.9.0/glanvu-0.9.0-linux-x86_64.tar.gz"
    sha256 "863d120743af4cf48158b0eef81ccb8d0c0c0a5568f3fd0f5bf997efcd264a8b"
    version "0.9.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.9.0/Glanvu-0.9.0-macos-arm64.zip"
      sha256 "12710851b445adee31029c622e6ceb4d501968c90ad2882ebeb3367a8b306878"
      version "0.9.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.9.0/Glanvu-0.9.0-macos-x86_64.zip"
      sha256 "6d4347e98a4ed9e88708abe45d54c12ddb2cbba9e2a17a425c5c6390edb9bd4d"
      version "0.9.0"
    end
  end

  def install
    if OS.mac?
      bin.install "Glanvu.app/Contents/MacOS/glanvu"
    else
      bin.install "glanvu"
    end
  end

  test do
    system bin/"glanvu", "--help"
  end
end
