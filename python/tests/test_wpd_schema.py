"""Tests for fakeprint.wpd_schema (WSD Print Service XSD model + body validation)."""



from __future__ import annotations



from pathlib import Path



import pytest



from fakeprint.wpd_schema import body_problems, load_schema



NS = "http://schemas.microsoft.com/windows/2006/08/wdp/print"

NS_01 = "http://schemas.microsoft.com/windows/2006/01/wdp/print"

SOAP = "http://www.w3.org/2003/05/soap-envelope"



ROOT = Path(__file__).resolve().parents[2]

SCHEMA_DIR = ROOT / "docs" / "reference" / "schema"

V20_XSD = ROOT / "docs" / "reference" / "upstream" / "print-v2" / "WDPPrintV20.xsd"





@pytest.fixture(scope="module", autouse=True)

def _require_schemas() -> None:

    if not SCHEMA_DIR.is_dir() or not any(SCHEMA_DIR.glob("*.xsd")):

        pytest.skip("run scripts/Get-WsdSpecs.ps1 to fetch print XSDs")





def envelope(body_inner: str, ns: str = NS, prefix: str = "wprt") -> str:

    return (

        f'<soap:Envelope xmlns:soap="{SOAP}" xmlns:{prefix}="{ns}">'

        f"<soap:Body>{body_inner}</soap:Body>"

        "</soap:Envelope>"

    )





def extension_envelope(xmlns: str, prefix: str, inner: str) -> str:

    return envelope(inner, xmlns, prefix)





def status_envelope(state: str, ns: str = NS) -> str:

    prefix = "wprt"

    return envelope(

        "<wprt:GetPrinterElementsResponse><wprt:PrinterElements>"

        '<wprt:ElementData Name="wprt:PrinterStatus" Valid="true">'

        "<wprt:PrinterStatus>"

        "<wprt:PrinterCurrentTime>2026-10-12T15:20:11Z</wprt:PrinterCurrentTime>"

        f"<wprt:PrinterState>{state}</wprt:PrinterState>"

        "<wprt:QueuedJobCount>0</wprt:QueuedJobCount>"

        "</wprt:PrinterStatus>"

        "</wprt:ElementData>"

        "</wprt:PrinterElements></wprt:GetPrinterElementsResponse>",

        ns=ns,

        prefix=prefix,

    )





def test_schema_lists_the_print_operations():

    schema = load_schema(SCHEMA_DIR)

    for name in (

        "CreatePrintJobRequest",

        "CreatePrintJobResponse",

        "GetPrinterElementsResponse",

        "SendDocumentResponse",

    ):

        assert name in schema.elements, name





def test_rejects_a_printer_state_outside_the_schema():

    problems = body_problems(status_envelope("Busy"))

    assert any("Busy" in problem for problem in problems), problems





def test_accepts_idle_status():

    assert body_problems(status_envelope("Idle")) == []





def test_accepts_idle_status_in_the_2006_01_namespace():

    xml = status_envelope("Idle", ns=NS_01)

    assert body_problems(xml) == []





def test_accepts_idle_printer_status_in_elements():

    assert body_problems(status_envelope("Idle")) == []





def test_create_print_job_response_may_carry_lab_job_token():

    xml = envelope(

        "<wprt:CreatePrintJobResponse>"

        "<wprt:JobId>1</wprt:JobId>"

        "<wprt:JobToken>job-1</wprt:JobToken>"

        "</wprt:CreatePrintJobResponse>"

    )

    assert body_problems(xml) == []





def test_printer_status_may_carry_lab_printer_local_time():

    xml = envelope(

        "<wprt:GetPrinterElementsResponse><wprt:PrinterElements>"

        '<wprt:ElementData Name="wprt:PrinterStatus" Valid="true">'

        "<wprt:PrinterStatus>"

        "<wprt:PrinterCurrentTime>2026-10-12T15:20:11Z</wprt:PrinterCurrentTime>"

        "<wprt:PrinterState>Idle</wprt:PrinterState>"

        "<wprt:QueuedJobCount>0</wprt:QueuedJobCount>"

        "<wprt:PrinterLocalTime>2026-01-01T00:00:00Z</wprt:PrinterLocalTime>"

        "</wprt:PrinterStatus>"

        "</wprt:ElementData>"

        "</wprt:PrinterElements></wprt:GetPrinterElementsResponse>"

    )

    assert body_problems(xml) == []





def test_job_status_schema_counters_are_not_scan_images():

    xml = envelope(

        "<wprt:GetJobElementsResponse><wprt:JobElements>"

        '<wprt:ElementData Name="wprt:JobStatus" Valid="true">'

        "<wprt:JobStatus>"

        "<wprt:JobId>1</wprt:JobId>"

        "<wprt:JobState>Pending</wprt:JobState>"

        "<wprt:JobStateReasons><wprt:JobStateReason>None</wprt:JobStateReason></wprt:JobStateReasons>"

        "<wprt:KOctetsProcessed>0</wprt:KOctetsProcessed>"

        "<wprt:MediaSheetsCompleted>0</wprt:MediaSheetsCompleted>"

        "<wprt:NumberOfDocuments>0</wprt:NumberOfDocuments>"

        "</wprt:JobStatus>"

        "</wprt:ElementData>"

        "</wprt:JobElements></wprt:GetJobElementsResponse>"

    )

    assert body_problems(xml) == []

    bad = xml.replace(

        "<wprt:NumberOfDocuments>0</wprt:NumberOfDocuments>",

        "<wprt:NumberOfDocuments>0</wprt:NumberOfDocuments><wprt:ImagesCompleted>1</wprt:ImagesCompleted>",

    )

    problems = body_problems(bad)

    assert any("ImagesCompleted" in problem for problem in problems), problems





def test_schema_lists_v11_and_v12_extensions():

    schema = load_schema(SCHEMA_DIR)

    for name in (

        "SupportsWSPrintv11",

        "DriverConfiguration",

        "ColorRepresentation",

        "SupportsWSPrintV12",

        "SetPrinterElementsRequest",

        "SetPrinterElementsResponse",

    ):

        assert name in schema.elements, name





def test_schema_lists_v20_when_the_package_was_fetched():

    if not V20_XSD.is_file():

        pytest.skip("WDPPrintV20.xsd not fetched")

    schema = load_schema(SCHEMA_DIR)

    for name in (

        "SupportsWSPrintV20",

        "GetPrintDeviceCapabilitiesRequest",

        "GetPrintDeviceCapabilitiesResponse",

        "CreatePrintJob2Request",

    ):

        assert name in schema.elements, name





def test_v11_driver_configuration_and_color_are_checked():

    ok = extension_envelope(

        "http://schemas.microsoft.com/windows/2010/06/wdp/printv11",

        "wprt11",

        "<wprt11:DriverConfiguration><wprt11:GPDConfigFile>printer.gpd</wprt11:GPDConfigFile></wprt11:DriverConfiguration>",

    )

    assert body_problems(ok) == []

    color = extension_envelope(

        "http://schemas.microsoft.com/windows/2010/06/wdp/printv11",

        "wprt11",

        "<wprt11:ColorRepresentation>000000</wprt11:ColorRepresentation>",

    )

    assert body_problems(color) == []

    short = color.replace("000000", "00000")

    problems = body_problems(short)

    assert any("ColorRepresentation" in problem for problem in problems), problems





def test_v12_set_printer_elements_request_is_checked():

    ok = extension_envelope(

        "http://schemas.microsoft.com/windows/2012/10/wdp/printV12",

        "wprt12",

        "<wprt12:SetPrinterElementsRequest><wprt12:ElementsToSet>"

        "<wprt12:ElementToSet>"

        "<wprt12:ElementPath>wprt:PrinterLocation</wprt12:ElementPath>"

        "<wprt12:String>lab</wprt12:String>"

        "</wprt12:ElementToSet>"

        "</wprt12:ElementsToSet></wprt12:SetPrinterElementsRequest>",

    )

    assert body_problems(ok) == []





def test_v20_capabilities_request_is_checked_when_fetched():

    if not V20_XSD.is_file():

        pytest.skip("WDPPrintV20.xsd not fetched")

    ok = extension_envelope(

        "http://schemas.microsoft.com/windows/2014/04/wdp/printV20",

        "wprt20",

        "<wprt20:GetPrintDeviceCapabilitiesRequest>"

        "<wprt20:PrintDeviceCapabilitiesFormat>application/vnd.ms-printer.pdc+xml</wprt20:PrintDeviceCapabilitiesFormat>"

        "<wprt20:PrintDeviceCapabilitiesChangeID>1</wprt20:PrintDeviceCapabilitiesChangeID>"

        "</wprt20:GetPrintDeviceCapabilitiesRequest>",

    )

    assert body_problems(ok) == []


