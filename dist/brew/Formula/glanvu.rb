class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.5.3/glanvu-0.5.3-linux-x86_64.tar.gz"
    sha256 "21396c517602c1599bb69aa6d875fa72b96afacaa2c9efe3c82f6fe615631235"
    version "0.5.3"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.5.3/Glanvu-0.5.3-macos-arm64.zip"
      sha256 "e18bd9cc414b4aca915cf9dd583f5a5d0f692b35817e29123b96d9e98e615d04"
      version "0.5.3"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.5.3/Glanvu-0.5.3-macos-x86_64.zip"
      sha256 "fdf888a36d74db1dc5c9502b5e2e19138a54c5cb6432b9214347647f7f0e26bf"
      version "0.5.3"
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
