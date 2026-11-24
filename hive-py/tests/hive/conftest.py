"""The hive-py root on sys.path, so `hive` and `scripts` import from any working directory."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
