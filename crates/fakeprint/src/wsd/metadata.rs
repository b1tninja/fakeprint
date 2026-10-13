//! Device metadata: WS-Transfer `Get` and WS-MetadataExchange `GetMetadata`.
//!
//! Both answer with a `wsx:Metadata` document built from [`DeviceInfo`]: a `ThisModel`
//! section, a `ThisDevice` section and a `Relationship` section whose `Host` is the device
//! (types `wsdp:Device wprt:PrintDeviceType`) and whose `Hosted` entry is the print service
//! (type `wprt:PrinterServiceType`) at its endpoint address. The PnP-X / Device Foundation
//! extras Windows needs to turn the device into a printer are on by default and can be
//! switched off (`DeviceInfo::windows_pnpx`).

use uuid::Uuid;

use super::envelope::EndpointRef;
use super::fault::PrintFault;
use super::soap::{SoapReply, SoapRequest};
use super::types::{EndpointKind, PrintOp};
use super::xml::{ns, Element, QName};

pub const DIALECT_THIS_MODEL: &str = "http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisModel";
pub const DIALECT_THIS_DEVICE: &str = "http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisDevice";
pub const DIALECT_RELATIONSHIP: &str = "http://schemas.xmlsoap.org/ws/2006/02/devprof/Relationship";
/// `Relationship/@Type` of the host relationship.
pub const RELATIONSHIP_HOST: &str = "http://schemas.xmlsoap.org/ws/2006/02/devprof/host";
pub const PNPX_NS: &str = "http://schemas.microsoft.com/windows/pnpx/2005/10";
pub const DEVICE_FOUNDATION_NS: &str =
    "http://schemas.microsoft.com/windows/2008/09/devicefoundation";

/// Which WS-Print versions the device advertises (informational for metadata; the flags
/// themselves live in `PrinterDescription`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrintVersions {
    pub v11: bool,
    pub v12: bool,
    pub v20: bool,
}

/// Everything the metadata answer is derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub manufacturer_url: Option<String>,
    pub model_name: String,
    pub model_number: Option<String>,
    pub model_url: Option<String>,
    /// Defaults to the origin of `device_endpoint` (a reachable URL, never `localhost`
    /// unless the device really is local).
    pub presentation_url: Option<String>,
    pub friendly_name: String,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
    pub uuid: Uuid,
    /// URL of the device (metadata) endpoint.
    pub device_endpoint: String,
    /// URL of the hosted print service.
    pub print_service_endpoint: String,
    pub print_versions: PrintVersions,
    /// Emit the PnP-X ids and device categories Windows needs to create a printer.
    pub windows_pnpx: bool,
    /// `pnpx:HardwareId` of the hosted service.
    pub hardware_id: String,
}

impl DeviceInfo {
    pub fn new(
        uuid: Uuid,
        device_endpoint: impl Into<String>,
        print_service_endpoint: impl Into<String>,
    ) -> Self {
        Self {
            manufacturer: "fakeprint".into(),
            manufacturer_url: None,
            model_name: "fakeprint".into(),
            model_number: None,
            model_url: None,
            presentation_url: None,
            friendly_name: "fakeprint".into(),
            firmware_version: None,
            serial_number: None,
            uuid,
            device_endpoint: device_endpoint.into(),
            print_service_endpoint: print_service_endpoint.into(),
            print_versions: PrintVersions::default(),
            windows_pnpx: true,
            hardware_id: "VEN_FAKE&DEV_WSDPRINT".into(),
        }
    }

    /// `urn:uuid:...` endpoint address of the device.
    pub fn device_address(&self) -> String {
        format!("urn:uuid:{}", self.uuid)
    }

    fn base_url(&self) -> String {
        origin_of(&self.device_endpoint)
    }

    /// `CompatibleId` of the hosted print service.
    pub fn compatible_id(&self) -> String {
        format!("{}/PrinterServiceType", ns::PRINT)
    }
}

/// `scheme://authority/` of a URL (the URL plus a slash when it has no path).
fn origin_of(url: &str) -> String {
    let url = url.trim();
    match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            let end = rest.find('/').unwrap_or(rest.len());
            format!("{}{}/", &url[..i + 3], &rest[..end])
        }
        None => format!("{url}/"),
    }
}

fn wsdp(local: &str) -> Element {
    Element::new(ns::WSDP, local)
}

fn lang_text(local: &str, value: &str) -> Element {
    Element::text_el(ns::WSDP, local, value).with_attr_ns(QName::new(ns::XML, "lang"), "en-US")
}

/// `ThisModel` section content.
pub fn this_model(info: &DeviceInfo) -> Element {
    let base = info.base_url();
    let mut el = wsdp("ThisModel").with_child(lang_text("Manufacturer", &info.manufacturer));
    el.children.push(Element::text_el(
        ns::WSDP,
        "ManufacturerUrl",
        info.manufacturer_url
            .clone()
            .unwrap_or_else(|| base.clone()),
    ));
    el.children.push(lang_text("ModelName", &info.model_name));
    if let Some(n) = &info.model_number {
        el.children
            .push(Element::text_el(ns::WSDP, "ModelNumber", n.as_str()));
    }
    el.children.push(Element::text_el(
        ns::WSDP,
        "ModelUrl",
        info.model_url.clone().unwrap_or_else(|| base.clone()),
    ));
    el.children.push(Element::text_el(
        ns::WSDP,
        "PresentationUrl",
        info.presentation_url.clone().unwrap_or(base),
    ));
    if info.windows_pnpx {
        el.children
            .push(Element::text_el(PNPX_NS, "DeviceCategory", "Printers"));
        el.children.push(Element::text_el(
            DEVICE_FOUNDATION_NS,
            "DeviceCategory",
            "PrintFax.Printer",
        ));
    }
    el
}

/// `ThisDevice` section content.
pub fn this_device(info: &DeviceInfo) -> Element {
    let mut el = wsdp("ThisDevice").with_child(lang_text("FriendlyName", &info.friendly_name));
    if let Some(v) = &info.firmware_version {
        el.children
            .push(Element::text_el(ns::WSDP, "FirmwareVersion", v.as_str()));
    }
    if let Some(v) = &info.serial_number {
        el.children
            .push(Element::text_el(ns::WSDP, "SerialNumber", v.as_str()));
    }
    el
}

/// `Relationship` section content: the device as host and the print service as hosted.
pub fn relationship(info: &DeviceInfo) -> Element {
    let host = wsdp("Host")
        .with_child(
            EndpointRef::new(info.device_address())
                .to_element(QName::new(ns::WSA, "EndpointReference")),
        )
        // the QName list below relies on the conventional prefixes (wsdp, wprt); the
        // hosted service's qtext guarantees the wprt declaration in the same document
        .with_child(Element::text_el(
            ns::WSDP,
            "Types",
            "wsdp:Device wprt:PrintDeviceType",
        ))
        .with_child(Element::text_el(
            ns::WSDP,
            "ServiceId",
            format!("uri:{}", info.uuid),
        ));
    let mut hosted = wsdp("Hosted")
        .with_child(
            EndpointRef::new(info.print_service_endpoint.as_str())
                .to_element(QName::new(ns::WSA, "EndpointReference")),
        )
        .with_child(wsdp("Types").with_qtext(QName::new(ns::PRINT, "PrinterServiceType")))
        .with_child(Element::text_el(
            ns::WSDP,
            "ServiceId",
            format!("uri:{}/PrinterService", info.uuid),
        ));
    if info.windows_pnpx {
        hosted.children.push(Element::text_el(
            PNPX_NS,
            "HardwareId",
            info.hardware_id.as_str(),
        ));
        hosted.children.push(Element::text_el(
            PNPX_NS,
            "CompatibleId",
            info.compatible_id(),
        ));
    }
    wsdp("Relationship")
        .with_attr("Type", RELATIONSHIP_HOST)
        .with_child(host)
        .with_child(hosted)
}

fn section(dialect: &str, content: Element) -> Element {
    Element::new(ns::MEX, "MetadataSection")
        .with_attr("Dialect", dialect)
        .with_child(content)
}

/// The full `wsx:Metadata` document, optionally restricted to one dialect.
pub fn metadata_document(info: &DeviceInfo, dialect: Option<&str>) -> Element {
    let all = [
        (DIALECT_THIS_MODEL, this_model as fn(&DeviceInfo) -> Element),
        (DIALECT_THIS_DEVICE, this_device),
        (DIALECT_RELATIONSHIP, relationship),
    ];
    let mut doc = Element::new(ns::MEX, "Metadata");
    for (d, build) in all {
        if dialect.is_none_or(|want| want == d) {
            doc.children.push(section(d, build(info)));
        }
    }
    doc
}

/// Handler for WS-Transfer Get and WS-MetadataExchange GetMetadata.
#[derive(Debug, Default, Clone, Copy)]
pub struct MetadataService;

impl MetadataService {
    /// Answer on the device endpoint.
    pub fn handle(&self, req: &SoapRequest, info: &DeviceInfo) -> SoapReply {
        self.handle_on(req, info, EndpointKind::Device)
    }

    /// Answer a request that arrived on `kind`. An operation the endpoint does not serve
    /// gets `wsa:ActionNotSupported`.
    pub fn handle_on(&self, req: &SoapRequest, info: &DeviceInfo, kind: EndpointKind) -> SoapReply {
        match self.answer(req, info, kind) {
            Ok(r) => r,
            Err(f) => req.fault_reply(&f),
        }
    }

    fn answer(
        &self,
        req: &SoapRequest,
        info: &DeviceInfo,
        kind: EndpointKind,
    ) -> Result<SoapReply, PrintFault> {
        let op = PrintOp::from_action(&req.action)
            .filter(|op| matches!(op, PrintOp::TransferGet | PrintOp::GetMetadata))
            .filter(|op| op.allowed_on(kind))
            .ok_or_else(|| PrintFault::action_not_supported(&req.action))?;
        req.check_must_understand(&[])?;
        let response_action = op.response_action().unwrap_or_default();
        match op {
            PrintOp::TransferGet => {
                // WS-Transfer Get has an empty body; anything else is not a Get.
                if let Some(b) = req.body() {
                    if !b.is(ns::WXF, "Get") {
                        return Err(PrintFault::malformed("WS-Transfer Get takes no body"));
                    }
                }
                Ok(req.reply(response_action, metadata_document(info, None)))
            }
            _ => {
                let body = req
                    .body()
                    .filter(|b| b.is(ns::MEX, "GetMetadata"))
                    .ok_or_else(|| PrintFault::malformed("the body is not wsx:GetMetadata"))?;
                let dialect_name = QName::new(ns::MEX, "Dialect");
                let mut dialects = body.find_all(&dialect_name);
                let dialect = dialects.next().map(|d| d.text_trimmed().to_string());
                if dialects.next().is_some() {
                    return Err(PrintFault::malformed("more than one wsx:Dialect"));
                }
                let identifier = body
                    .child(ns::MEX, "Identifier")
                    .map(|i| i.text_trimmed().to_string());
                let mut doc = metadata_document(info, dialect.as_deref());
                if identifier.is_some() {
                    // none of our sections carries an identifier
                    doc.children.clear();
                }
                Ok(req.reply(response_action, doc))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::xml::{parse_str, XmlLimits};

    fn info() -> DeviceInfo {
        let mut i = DeviceInfo::new(
            Uuid::parse_str("00000000-0000-4000-8000-0000000000f3").expect("uuid"),
            "http://192.168.1.50:3921/WebServices/Device",
            "http://192.168.1.50:3921/WebServices/PrinterService",
        );
        i.manufacturer = "Acme".into();
        i.model_name = "Press 9".into();
        i.serial_number = Some("SN1".into());
        i.firmware_version = Some("1.0".into());
        i.friendly_name = "Acme Press".into();
        i
    }

    fn request(action: &str, body: &str) -> SoapRequest {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{}" xmlns:a="{}" xmlns:x="{}" xmlns:m="{}"><s:Header><a:To>urn:uuid:00000000-0000-4000-8000-0000000000f3</a:To><a:Action>{action}</a:Action><a:MessageID>urn:uuid:22222222-2222-4222-8222-222222222222</a:MessageID></s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
            ns::SOAP12,
            ns::WSA,
            ns::WXF,
            ns::MEX
        );
        SoapRequest::parse(Some("application/soap+xml"), None, xml.as_bytes()).expect("parse")
    }

    const GET: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get";
    const GETMD: &str = "http://schemas.xmlsoap.org/ws/2004/09/mex/GetMetadata/Request";

    fn kids(el: &Element) -> Vec<String> {
        el.children.iter().map(|c| c.name.local.clone()).collect()
    }

    #[test]
    fn transfer_get_has_three_sections_in_order() {
        let r = MetadataService.handle(&request(GET, ""), &info());
        assert_eq!(
            r.action,
            "http://schemas.xmlsoap.org/ws/2004/09/transfer/GetResponse"
        );
        assert_eq!(
            r.relates_to.as_deref(),
            Some("urn:uuid:22222222-2222-4222-8222-222222222222")
        );
        let doc = r.body.expect("body");
        assert_eq!(doc.name, QName::new(ns::MEX, "Metadata"));
        let dialects: Vec<&str> = doc
            .children
            .iter()
            .map(|s| s.attr_local("Dialect").expect("d"))
            .collect();
        assert_eq!(
            dialects,
            [
                DIALECT_THIS_MODEL,
                DIALECT_THIS_DEVICE,
                DIALECT_RELATIONSHIP
            ]
        );
    }

    #[test]
    fn this_model_and_device_shapes() {
        let i = info();
        let m = this_model(&i);
        assert_eq!(
            kids(&m),
            [
                "Manufacturer",
                "ManufacturerUrl",
                "ModelName",
                "ModelUrl",
                "PresentationUrl",
                "DeviceCategory",
                "DeviceCategory"
            ]
        );
        assert_eq!(
            m.child(ns::WSDP, "PresentationUrl").expect("p").text,
            "http://192.168.1.50:3921/"
        );
        assert_eq!(
            m.children[0].attr(&QName::new(ns::XML, "lang")),
            Some("en-US")
        );
        assert_eq!(
            m.child(PNPX_NS, "DeviceCategory").expect("c").text,
            "Printers"
        );
        let mut plain = i.clone();
        plain.windows_pnpx = false;
        plain.model_number = Some("9".into());
        assert_eq!(
            kids(&this_model(&plain)),
            [
                "Manufacturer",
                "ManufacturerUrl",
                "ModelName",
                "ModelNumber",
                "ModelUrl",
                "PresentationUrl"
            ]
        );
        let d = this_device(&i);
        assert_eq!(
            kids(&d),
            ["FriendlyName", "FirmwareVersion", "SerialNumber"]
        );
    }

    #[test]
    fn relationship_host_and_hosted() {
        let i = info();
        let rel = relationship(&i);
        assert_eq!(rel.attr_local("Type"), Some(RELATIONSHIP_HOST));
        assert_eq!(kids(&rel), ["Host", "Hosted"]);
        let host = &rel.children[0];
        assert_eq!(kids(host), ["EndpointReference", "Types", "ServiceId"]);
        assert_eq!(
            host.children[0].child(ns::WSA, "Address").expect("a").text,
            "urn:uuid:00000000-0000-4000-8000-0000000000f3"
        );
        let hosted = &rel.children[1];
        assert_eq!(
            kids(hosted),
            [
                "EndpointReference",
                "Types",
                "ServiceId",
                "HardwareId",
                "CompatibleId"
            ]
        );
        assert_eq!(
            hosted.children[0]
                .child(ns::WSA, "Address")
                .expect("a")
                .text,
            i.print_service_endpoint
        );
        assert_eq!(
            hosted.children[1].text_qname(),
            Some(QName::new(ns::PRINT, "PrinterServiceType"))
        );
        assert_eq!(
            hosted.child(PNPX_NS, "CompatibleId").expect("c").text,
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterServiceType"
        );
    }

    #[test]
    fn serialized_types_resolve_after_reparse() {
        let r = MetadataService.handle(&request(GET, ""), &info());
        let xml = r.to_xml();
        let root = parse_str(&xml, &XmlLimits::SMALL).expect("reparse");
        let body = root.child(ns::SOAP12, "Body").expect("body");
        let host_types = body
            .descendants()
            .into_iter()
            .find(|e| e.is(ns::WSDP, "Types") && e.text.contains("Device"))
            .expect("host types");
        let names: Vec<QName> = host_types
            .text
            .split_whitespace()
            .map(|t| {
                let (p, l) = t.split_once(':').expect("prefixed");
                QName::new(&host_types.resolve_prefix(p).expect("prefix bound"), l)
            })
            .collect();
        assert_eq!(
            names,
            [
                QName::new(ns::WSDP, "Device"),
                QName::new(ns::PRINT, "PrintDeviceType")
            ]
        );
    }

    #[test]
    fn get_metadata_filters_by_dialect() {
        let svc = MetadataService;
        let all = svc.handle(&request(GETMD, "<m:GetMetadata/>"), &info());
        assert_eq!(
            all.action,
            "http://schemas.xmlsoap.org/ws/2004/09/mex/GetMetadata/Response"
        );
        assert_eq!(all.body.expect("b").children.len(), 3);
        let one = svc.handle(
            &request(
                GETMD,
                &format!(
                    "<m:GetMetadata><m:Dialect>{DIALECT_THIS_DEVICE}</m:Dialect></m:GetMetadata>"
                ),
            ),
            &info(),
        );
        let doc = one.body.expect("b");
        assert_eq!(doc.children.len(), 1);
        assert_eq!(
            doc.children[0].attr_local("Dialect"),
            Some(DIALECT_THIS_DEVICE)
        );
        let none = svc.handle(
            &request(
                GETMD,
                "<m:GetMetadata><m:Dialect>urn:nope</m:Dialect></m:GetMetadata>",
            ),
            &info(),
        );
        assert!(none.body.expect("b").children.is_empty());
        let ident = svc.handle(
            &request(
                GETMD,
                "<m:GetMetadata><m:Identifier>urn:x</m:Identifier></m:GetMetadata>",
            ),
            &info(),
        );
        assert!(ident.body.expect("b").children.is_empty());
    }

    fn fault_local(r: &SoapReply) -> String {
        let b = r.body.as_ref().expect("b");
        assert_eq!(b.name.local, "Fault");
        let code = b.child(ns::SOAP12, "Code").expect("c");
        match code.child(ns::SOAP12, "Subcode") {
            Some(s) => {
                s.child(ns::SOAP12, "Value")
                    .expect("v")
                    .text_qname()
                    .expect("q")
                    .local
            }
            None => {
                code.child(ns::SOAP12, "Value")
                    .expect("v")
                    .text_qname()
                    .expect("q")
                    .local
            }
        }
    }

    #[test]
    fn faults_for_bad_bodies_and_wrong_endpoint() {
        let svc = MetadataService;
        assert_eq!(
            fault_local(&svc.handle(&request(GETMD, "<m:Other/>"), &info())),
            "Sender"
        );
        assert_eq!(
            fault_local(&svc.handle(&request(GETMD, ""), &info())),
            "Sender"
        );
        assert_eq!(
            fault_local(&svc.handle(&request(GET, "<m:Junk/>"), &info())),
            "Sender"
        );
        assert_eq!(
            fault_local(&svc.handle(&request(GETMD, "<m:GetMetadata><m:Dialect>a</m:Dialect><m:Dialect>b</m:Dialect></m:GetMetadata>"), &info())),
            "Sender"
        );
        assert_eq!(
            fault_local(&svc.handle(&request("urn:other", ""), &info())),
            "ActionNotSupported"
        );
        // Get is not served by the discovery endpoint
        let r = svc.handle_on(&request(GET, ""), &info(), EndpointKind::Discovery);
        assert_eq!(fault_local(&r), "ActionNotSupported");
        // but the print service answers it
        let r = svc.handle_on(&request(GET, ""), &info(), EndpointKind::PrintService);
        assert_eq!(r.body.expect("b").name.local, "Metadata");
    }

    #[test]
    fn explicit_urls_win_over_derived_ones() {
        let mut i = info();
        i.presentation_url = Some("http://printer.example/ui".into());
        i.model_url = Some("http://printer.example/m".into());
        i.manufacturer_url = Some("http://printer.example/".into());
        let m = this_model(&i);
        assert_eq!(
            m.child(ns::WSDP, "PresentationUrl").expect("p").text,
            "http://printer.example/ui"
        );
        assert_eq!(
            m.child(ns::WSDP, "ModelUrl").expect("p").text,
            "http://printer.example/m"
        );
        assert_eq!(origin_of("http://[::1]:80/a/b"), "http://[::1]:80/");
    }
}
