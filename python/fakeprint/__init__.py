"""fakeprint control-plane client (optional lab harness)."""

from .client import CapturedJob, Client, Status

__all__ = ["Client", "Status", "CapturedJob", "__version__"]
__version__ = "0.1.0"
