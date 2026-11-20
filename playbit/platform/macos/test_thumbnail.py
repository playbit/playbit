"""Validate the containing app's document registration and extension metadata."""
import importlib.util
from pathlib import Path
import plistlib
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("thumbnail", SOURCE / "build-thumbnail.py")
thumbnail = importlib.util.module_from_spec(spec)
spec.loader.exec_module(thumbnail)


class ThumbnailPackagingTests(unittest.TestCase):
    def test_document_owner_and_extension_share_the_exact_type(self):
        # Both debug and release must support Finder launch and thumbnail discovery.
        template = (SOURCE / "Info.plist").read_text()
        for identifier in ("playbit.engine", "playbit.engine.debug"):
            info = plistlib.loads(template.replace("@PLAYBIT_APP_ID@", identifier).encode())
            with tempfile.TemporaryDirectory() as directory:
                app = Path(directory) / "Playbit.app"
                (app / "Contents").mkdir(parents=True)
                (app / "Contents/Info.plist").write_bytes(plistlib.dumps(info))
                extension = thumbnail.configure(app)
            self.assertEqual(extension["CFBundleIdentifier"], identifier + ".thumbnail")
            self.assertEqual(extension["LSMinimumSystemVersion"], info["LSMinimumSystemVersion"])
            attributes = extension["NSExtension"]["NSExtensionAttributes"]
            self.assertEqual(attributes["QLSupportedContentTypes"], ["app.playbit.app"])
            self.assertEqual(attributes["QLThumbnailMinimumDimension"], 0)
            declared = next(item for item in info["UTExportedTypeDeclarations"]
                            if item["UTTypeIdentifier"] == "app.playbit.app")
            self.assertEqual(declared["UTTypeConformsTo"], ["public.data", "public.content"])
            self.assertEqual(declared["UTTypeTagSpecification"]["public.filename-extension"], ["pbapp"])
            document = next(item for item in info["CFBundleDocumentTypes"]
                            if item["LSItemContentTypes"] == ["app.playbit.app"])
            self.assertEqual(document["CFBundleTypeRole"], "Viewer")
            self.assertEqual(document["LSHandlerRank"], "Owner")
            self.assertEqual(document["CFBundleTypeIconFile"], declared["UTTypeIconFile"])


if __name__ == "__main__":
    unittest.main()
