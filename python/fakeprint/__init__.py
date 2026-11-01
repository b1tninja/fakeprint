"""fakeprint control-plane client (optional lab harness)."""

from .client import CapturedJob, Client, Status
from .wpd_docs import generate_reference_docs
from .wpd_schema import body_problems, load_schema

__all__ = [
    "Client",
    "Status",
    "CapturedJob",
    "body_problems",
    "generate_reference_docs",
    "load_schema",
    "__version__",
]
__version__ = "0.1.0"
