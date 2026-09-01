import unittest

from cast_common import disable_applet, enable_applet, is_applet_enabled, preferred_panel


class AppletPlacementTests(unittest.TestCase):
    def test_detects_and_disables_only_cast_applet(self):
        entries = [
            "panel1:left:0:menu@cinnamon.org:1",
            "panel1:right:2:cast@cast-rs:7",
            "panel1:right:3:calendar@cinnamon.org:2",
        ]
        self.assertTrue(is_applet_enabled(entries))
        self.assertEqual(
            disable_applet(entries),
            ["panel1:left:0:menu@cinnamon.org:1", "panel1:right:3:calendar@cinnamon.org:2"],
        )

    def test_enables_on_first_panel_after_existing_right_items(self):
        entries = ["panel2:right:3:calendar@cinnamon.org:2"]
        updated, next_id = enable_applet(entries, ["2:0:top"], 9)
        self.assertEqual(updated[-1], "panel2:right:4:cast@cast-rs:9")
        self.assertEqual(next_id, 10)

    def test_enable_is_idempotent(self):
        entries = ["panel1:right:0:cast@cast-rs:4"]
        updated, next_id = enable_applet(entries, [], 5)
        self.assertEqual(updated, entries)
        self.assertEqual(next_id, 5)

    def test_panel_fallback(self):
        self.assertEqual(preferred_panel([]), "panel1")
        self.assertEqual(preferred_panel(["unexpected"]), "panel1")


if __name__ == "__main__":
    unittest.main()
