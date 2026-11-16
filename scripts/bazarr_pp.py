#!/usr/bin/env python3
# Runs inside the linuxserver Bazarr image: feed a post-processing command
# through Bazarr's own pp_replace() and postprocessing(), exactly as Bazarr
# does after downloading a subtitle.
import sys, types, logging
sys.path.insert(0, "/app/bazarr/bin/bazarr")
# utilities.post_processing imports app.config.settings; stub it.
app = types.ModuleType("app"); cfg = types.ModuleType("app.config")
cfg.settings = types.SimpleNamespace(general=types.SimpleNamespace(chmod="0640", chmod_enabled=False))
app.config = cfg; sys.modules["app"] = app; sys.modules["app.config"] = cfg
logging.basicConfig(level=logging.INFO, format="BAZARR-LOG %(levelname)s %(message)s")
from utilities.post_processing import pp_replace
import importlib.util
spec = importlib.util.spec_from_file_location("bazarr_pp_exec", "/app/bazarr/bin/bazarr/subtitles/post_processing.py")
mod = importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)
postprocessing = mod.postprocessing

cmd, episode, subtitles = sys.argv[1], sys.argv[2], sys.argv[3]
args = pp_replace(cmd, episode, subtitles, "Dutch", "nl", "dut", "English", "en", "eng",
                  "95", "123", "opensubtitles", "someone", "release", "1", "2")
print("argv:", args)
postprocessing(args, episode)
