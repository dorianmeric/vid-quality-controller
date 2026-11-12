cargo build --release
New-Item -ItemType Directory -Force -Path dist\app
Copy-Item target\release\vid-quality-controller.exe dist\app

# Copy runtime assets, docs, or required .dll files if needed
Copy-Item README.md, LICENSE, config.json dist\app

# Create the zip archive
Compress-Archive -Path dist\app\* -DestinationPath dist\vid-quality-controller-release.zip -Force

