#!/usr/bin/env python3
"""Build the macOS replicated extension, on-demand service and drive coordinator."""
import fnmatch
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys

SOURCE = Path(__file__).resolve().parent / "FileProvider"
ASSOCIATED_DOMAINS = ["applinks:playbit.app", "applinks:testing.playbit.app"]


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(plistlib.dumps(value))


def compile_swift(output, files, module, extension=False, library=False):
    output.parent.mkdir(parents=True, exist_ok=True)
    arguments = ["xcrun", "swiftc", "-swift-version", "5", "-O", "-module-name", module,
        "-module-cache-path", str(Path("target/swift-provider-modules").resolve()),
        "-target", "arm64-apple-macosx15.0", "-o", str(output)]
    if extension:
        arguments += ["-application-extension", "-parse-as-library", "-Xlinker", "-e", "-Xlinker", "_NSExtensionMain"]
    if library:
        arguments += ["-emit-library", "-parse-as-library"]
    subprocess.run(arguments + [str(SOURCE / ("../../apple/FileProvider/" + name if name in {"Extension.swift", "Enumerator.swift", "Item.swift", "Operation.swift"} else name)) for name in files], check=True)


def profile(path):
    keychain = Path.home() / "Library/Keychains/login.keychain-db"
    return plistlib.loads(subprocess.check_output(["security", "cms", "-D", "-k", str(keychain), "-i", path]))


def sign(bundle, entitlements, identity):
    command = ["codesign", "--force", "--sign", identity, "--entitlements", str(entitlements)]
    if identity != "-":
        command += ["--timestamp", "--options", "runtime"]
    subprocess.run(command + [str(bundle)], check=True)


def build(app, output):
    contents = app / "Contents"
    info = plistlib.loads((contents / "Info.plist").read_bytes())
    identifier = info["CFBundleIdentifier"]
    provider_id = identifier + ".fileprovider"
    service_id = provider_id + ".service"
    group = "group.app.playbit" + (".debug" if identifier.endswith(".debug") else "")
    extension = contents / "PlugIns/PlaybitFileProvider.appex"
    service = extension / "Contents/XPCServices/PlaybitProviderService.xpc"
    version = info["CFBundleVersion"]
    common = {"CFBundleVersion": version, "CFBundleShortVersionString": info["CFBundleShortVersionString"],
        "LSMinimumSystemVersion": "15.0"}
    write(extension / "Contents/Info.plist", {**common, "CFBundleIdentifier": provider_id,
        "CFBundleName": "Playbit", "CFBundleDisplayName": "Playbit", "CFBundlePackageType": "XPC!",
        "CFBundleExecutable": "PlaybitFileProvider", "PlaybitProviderService": service_id,
        "UTImportedTypeDeclarations": info["UTExportedTypeDeclarations"],
        "PlaybitContainingAppIdentifier": identifier,
        "NSExtension": {"NSExtensionPointIdentifier": "com.apple.fileprovider-nonui",
            "NSExtensionPrincipalClass": "FileProviderExtension",
            "NSExtensionFileProviderSupportsEnumeration": True,
            "NSExtensionFileProviderDocumentGroup": group,
            "NSExtensionFileProviderDownloadPipelineDepth": 2,
            "NSExtensionFileProviderUploadPipelineDepth": 2,
            "NSExtensionFileProviderMetadataOnlyUploadPipelineDepth": 2}})
    write(service / "Contents/Info.plist", {**common, "CFBundleIdentifier": service_id,
        "CFBundleName": "Playbit Provider Service", "CFBundlePackageType": "XPC!",
        "CFBundleExecutable": "PlaybitProviderService", "XPCService": {"ServiceType": "Application"}})
    compile_swift(extension / "Contents/MacOS/PlaybitFileProvider",
        ["../../ios/Host/Identifier.swift", "Bridge.swift", "Operation.swift", "Item.swift", "Enumerator.swift", "Extension.swift"], "PlaybitFileProvider", True)
    compile_swift(service / "Contents/MacOS/PlaybitProviderService", ["Service.swift"], "PlaybitProviderService")
    compile_swift(contents / "Frameworks/PlaybitDrives.dylib", ["../../ios/Host/Identifier.swift", "Drives.swift"], "PlaybitDrives", library=True)
    shutil.copy2(app.parent / "playbit-file-provider", contents / "MacOS/playbit-file-provider")
    identity = os.environ.get("PB_SIGN_IDENTITY", "-")
    host = plistlib.loads((contents / "Resources/runtime-entitlements.plist").read_bytes())
    host["com.apple.security.application-groups"] = [group]
    provider = {"com.apple.security.app-sandbox": True,
        "com.apple.security.application-groups": [group]}
    service_entitlements = {}
    configured = os.environ.get("PB_PROVISIONING_PROFILE")
    requirement = 'identifier "{}"'
    if configured:
        main = profile(configured)
        requirement += ' and anchor apple generic and certificate leaf[subject.OU] = "' + main["TeamIdentifier"][0] + '"'
        permitted = main["Entitlements"]
        application_id = main["ApplicationIdentifierPrefix"][0] + "." + identifier
        if not fnmatch.fnmatchcase(application_id, permitted.get("com.apple.application-identifier", "")):
            raise ValueError("App profile does not authorize the application identifier")
        domains = permitted.get("com.apple.developer.associated-domains", [])
        # Apple profiles can grant the capability with a single wildcard string.
        if domains != "*" and (not isinstance(domains, list) or not all(
            any(isinstance(pattern, str) and fnmatch.fnmatchcase(domain, pattern) for pattern in domains)
            for domain in ASSOCIATED_DOMAINS
        )):
            raise ValueError("App profile must authorize Associated Domains for playbit.app and testing.playbit.app")
        push = permitted.get("com.apple.developer.aps-environment")
        if group not in permitted.get("com.apple.security.application-groups", []) or push not in ("development", "production"):
            raise ValueError("App profile must authorize the File Provider app group and push notifications")
        host.update({"com.apple.application-identifier": application_id,
            "com.apple.developer.team-identifier": main["TeamIdentifier"][0],
            "com.apple.developer.associated-domains": ASSOCIATED_DOMAINS,
            "com.apple.developer.aps-environment": push})
        shutil.copyfile(configured, contents / "embedded.provisionprofile")
        extension_profile = os.environ.get("PB_FILE_PROVIDER_PROVISIONING_PROFILE")
        if not extension_profile:
            raise ValueError("PB_FILE_PROVIDER_PROVISIONING_PROFILE is required for provisioned builds")
        selected = profile(extension_profile)
        if selected["TeamIdentifier"] != main["TeamIdentifier"]:
            raise ValueError("File Provider and app profiles must use the same company team")
        extension_id = selected["ApplicationIdentifierPrefix"][0] + "." + provider_id
        if not fnmatch.fnmatchcase(extension_id, selected["Entitlements"].get("com.apple.application-identifier", "")):
            raise ValueError("File Provider profile does not authorize the extension identifier")
        if group not in selected["Entitlements"].get("com.apple.security.application-groups", []):
            raise ValueError("File Provider profile does not authorize the app group")
        provider.update({"com.apple.application-identifier": extension_id,
            "com.apple.developer.team-identifier": selected["TeamIdentifier"][0]})
        shutil.copyfile(extension_profile, extension / "Contents/embedded.provisionprofile")
        info["PlaybitPushEnvironment"] = push
    else:
        info["PlaybitPushEnvironment"] = "development"
    extension_info = plistlib.loads((extension / "Contents/Info.plist").read_bytes())
    extension_info["PlaybitPushEnvironment"] = info["PlaybitPushEnvironment"]
    extension_info["PlaybitProviderServiceRequirement"] = requirement.format(service_id)
    service_info = plistlib.loads((service / "Contents/Info.plist").read_bytes())
    service_info["PlaybitProviderClientRequirement"] = requirement.format(provider_id)
    write(service / "Contents/Info.plist", service_info)
    write(extension / "Contents/Info.plist", extension_info)
    write(contents / "Info.plist", info)
    output.mkdir(parents=True, exist_ok=True)
    write(output / "provider.plist", provider)
    write(output / "provider-service.plist", service_entitlements)
    write(output / "provider-host.plist", host)
    sign(service, output / "provider-service.plist", identity)
    sign(extension, output / "provider.plist", identity)
    sign(contents / "MacOS/playbit-file-provider", output / "provider-service.plist", identity)
    sign(contents / "Frameworks/PlaybitDrives.dylib", output / "provider-service.plist", identity)


if __name__ == "__main__":
    build(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
