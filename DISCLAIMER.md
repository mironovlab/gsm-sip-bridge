# Disclaimer

**This project is for educational and personal/hobbyist purposes only.**
It is not intended for, and must not be used in, any commercial product,
service, or deployment.

Bridging cellular calls (GSM, VoWiFi, VoLTE) to SIP/VoIP can be subject to
telecom regulations, spectrum/interconnection rules, and your cellular
provider's terms of service — including restrictions on unlicensed GSM
gateways / "SIM boxes" and call-traffic bypass, which are illegal in many
countries. **Before using this software, check your local laws and your
cellular provider's terms and conditions** to confirm your intended use is
permitted. Running multiple SIMs through a single automated system can
also trigger a carrier's anti-fraud detection even for personal use,
independent of the legal question.

**You use this software entirely at your own risk.** It is provided "AS
IS", without warranty of any kind, express or implied (see
[LICENSE](LICENSE)). The author and contributors are not responsible for
any damage, loss, fines, service or account termination, legal liability,
or other harm arising from its use, misuse, or inability to use.

This project is not affiliated with, endorsed by, or sponsored by
Quectel, Sangoma/Asterisk, FreePBX, or any cellular carrier; product
names are used solely to describe interoperability.

Exposing the optional built-in SIP server or RTP media to any untrusted
network is a toll-fraud risk — securing those endpoints (firewalling,
authentication, rate limiting) is entirely the operator's responsibility.

Support via [Discussions](https://github.com/selvakn/gsm-sip-bridge/discussions)
and [Issues](https://github.com/selvakn/gsm-sip-bridge/issues) is volunteer,
best-effort, with no guaranteed response time or SLA.

The VoWiFi/IMS path bundles IPsec/IKEv2 encryption (via strongSwan).
Importing, exporting, or using this software may be subject to
encryption import/export control laws in your country — it is your
responsibility to comply with them.
