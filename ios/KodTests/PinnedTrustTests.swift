//  PinnedTrustTests.swift — the delegate, not just the arithmetic.
//
//  Every other pinning test drives `KeyPin` directly, which leaves the code that
//  actually answers a TLS challenge untested: an adversarial review mutated
//  `PinnedTrust` to call `.useCredential` unconditionally — accept every server on
//  earth — and the whole suite stayed green. These tests exercise the delegate
//  method itself, so that mutation fails.
//
//  The two certificates below are real, self-signed, minted with /usr/bin/openssl,
//  and their pins were computed the long way:
//    openssl x509 -pubkey -noout | openssl pkey -pubin -outform DER \
//      | openssl dgst -sha256 -binary | base64 | tr '+/' '-_' | tr -d '='

import XCTest
@testable import Kod

/// A protection space that can carry a SecTrust. `URLProtectionSpace` has no
/// initialiser that takes one, and subclassing is the documented way in.
private final class TrustSpace: URLProtectionSpace {
    private let trust: SecTrust?
    init(trust: SecTrust?) {
        self.trust = trust
        super.init(host: "kod.test", port: 8787, protocol: "https",
                   realm: nil, authenticationMethod: NSURLAuthenticationMethodServerTrust)
    }
    required init?(coder: NSCoder) { fatalError() }
    override var serverTrust: SecTrust? { trust }
}

private final class NullSender: NSObject, URLAuthenticationChallengeSender {
    func use(_ credential: URLCredential, for challenge: URLAuthenticationChallenge) {}
    func continueWithoutCredential(for challenge: URLAuthenticationChallenge) {}
    func cancel(_ challenge: URLAuthenticationChallenge) {}
}

final class PinnedTrustTests: XCTestCase {
    private static let certA = "MIICDzCCAbQCCQDPY46huJgJHDAKBggqhkjOPQQDAjAVMRMwEQYDVQQDDAprb2QtdGVzdC1hMB4XDTI2MDgyNjAwMjYzOVoXDTM2MDgyMzAwMjYzOVowFTETMBEGA1UEAwwKa29kLXRlc3QtYTCCAUswggEDBgcqhkjOPQIBMIH3AgEBMCwGByqGSM49AQECIQD/////AAAAAQAAAAAAAAAAAAAAAP///////////////zBbBCD/////AAAAAQAAAAAAAAAAAAAAAP///////////////AQgWsY12Ko6k+ez671VdpiGvGUdBrDMU7D2O848PifSYEsDFQDEnTYIhucEk2pmeOETnSa3gZ9+kARBBGsX0fLhLEJH+Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT+NC4v4af5uO5+tKfA+eFivOM1drMV7Oy7ZAaDe/UfUCIQD/////AAAAAP//////////vOb6racXnoTzucrC/GMlUQIBAQNCAATDSApSvgLdGr34K64K5JfUt1zjFz9fPLbHV4T/fmgNOeja/D0iJyeSXkT3FcVkLwo/BuK+wG3FPhdXK16kLv5KMAoGCCqGSM49BAMCA0kAMEYCIQCyIC51sYg1xg28xVgOuRsA8GbQh3C0fkXAhDOeuLsAqwIhAMCfmdzAYXolQ2nBK2pB4F+289C9TuB7tJe9LlX8YEqS"
    private static let pinA = "U73rrP-WlRVncJ5u11zC4nSQ429fRW3QjKvODKBOlb4"
    private static let certB = "MIICDzCCAbQCCQDfTiaDKIvBkDAKBggqhkjOPQQDAjAVMRMwEQYDVQQDDAprb2QtdGVzdC1iMB4XDTI2MDgyNjAwMjYzOVoXDTM2MDgyMzAwMjYzOVowFTETMBEGA1UEAwwKa29kLXRlc3QtYjCCAUswggEDBgcqhkjOPQIBMIH3AgEBMCwGByqGSM49AQECIQD/////AAAAAQAAAAAAAAAAAAAAAP///////////////zBbBCD/////AAAAAQAAAAAAAAAAAAAAAP///////////////AQgWsY12Ko6k+ez671VdpiGvGUdBrDMU7D2O848PifSYEsDFQDEnTYIhucEk2pmeOETnSa3gZ9+kARBBGsX0fLhLEJH+Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT+NC4v4af5uO5+tKfA+eFivOM1drMV7Oy7ZAaDe/UfUCIQD/////AAAAAP//////////vOb6racXnoTzucrC/GMlUQIBAQNCAATJu68JbVQRdo/BtVhgU35EWpsujT7UnCP3wADfALYJeWHLNya0ncRiVdHdBknsi1Rgy5LAZcH7d1zugMMqglcYMAoGCCqGSM49BAMCA0kAMEYCIQCNmfgWF/bLupEk8RJ+LTpaAT63QrOsICOx47w2wbXjaAIhALW6odl64YOrnL4+vePtLf9steE5VOA6trlXndvHSsG2"
    private static let pinB = "CUPSS_h3yxOVieg4r0iuNZgFEs_XwUv2b6nzNUyxTnA"

    private func trust(_ b64: String) -> SecTrust {
        let der = Data(base64Encoded: b64)!
        let cert = SecCertificateCreateWithData(nil, der as CFData)!
        var t: SecTrust?
        SecTrustCreateWithCertificates(cert, SecPolicyCreateBasicX509(), &t)
        return t!
    }

    /// Drive the delegate exactly as URLSession does, and report what it decided.
    private func answer(pin: String, trust: SecTrust?) -> (URLSession.AuthChallengeDisposition, URLCredential?) {
        let d = PinnedTrust(expected: pin)
        d.arm()
        let challenge = URLAuthenticationChallenge(
            protectionSpace: TrustSpace(trust: trust),
            proposedCredential: nil, previousFailureCount: 0,
            failureResponse: nil, error: nil, sender: NullSender())
        var got: (URLSession.AuthChallengeDisposition, URLCredential?)!
        let done = expectation(description: "challenge answered")
        d.urlSession(URLSession.shared, didReceive: challenge) { disposition, credential in
            got = (disposition, credential)
            done.fulfill()
        }
        wait(for: [done], timeout: 2)
        return got
    }

    func testTheDelegateAcceptsOnlyThePinnedKey() {
        let (disposition, credential) = answer(pin: Self.pinA, trust: trust(Self.certA))
        XCTAssertEqual(disposition, .useCredential)
        XCTAssertNotNil(credential, "an acceptance must carry the credential")
    }

    /// THE test. A different Mac — or something pretending to be one — presents a
    /// perfectly valid self-signed certificate for a key we never paired with.
    func testTheDelegateRefusesADifferentKey() {
        let (disposition, credential) = answer(pin: Self.pinA, trust: trust(Self.certB))
        XCTAssertEqual(disposition, .cancelAuthenticationChallenge,
                       "a key we did not pair with must be REFUSED, not accepted")
        XCTAssertNil(credential)
    }

    func testARefusalIsReportedAsASecurityEventNotSilence() {
        let d = PinnedTrust(expected: Self.pinA)
        d.arm()
        XCTAssertNil(d.refusal, "armed means no stale verdict")
        let challenge = URLAuthenticationChallenge(
            protectionSpace: TrustSpace(trust: trust(Self.certB)),
            proposedCredential: nil, previousFailureCount: 0,
            failureResponse: nil, error: nil, sender: NullSender())
        let done = expectation(description: "answered")
        d.urlSession(URLSession.shared, didReceive: challenge) { _, _ in done.fulfill() }
        wait(for: [done], timeout: 2)
        XCTAssertNotNil(d.refusal, "the user must be told why, not left watching a retry loop")
    }

    func testAChallengeWithNoTrustIsRefused() {
        let (disposition, credential) = answer(pin: Self.pinA, trust: nil)
        XCTAssertEqual(disposition, .cancelAuthenticationChallenge)
        XCTAssertNil(credential)
    }

    /// A non-server-trust challenge is not ours to answer — and must not be an
    /// accept either.
    func testANonServerTrustChallengeIsPassedThroughRatherThanAccepted() {
        let d = PinnedTrust(expected: Self.pinA)
        let space = URLProtectionSpace(host: "kod.test", port: 8787, protocol: "https",
                                       realm: nil, authenticationMethod: NSURLAuthenticationMethodHTTPBasic)
        let challenge = URLAuthenticationChallenge(
            protectionSpace: space, proposedCredential: nil, previousFailureCount: 0,
            failureResponse: nil, error: nil, sender: NullSender())
        var disposition: URLSession.AuthChallengeDisposition!
        let done = expectation(description: "answered")
        d.urlSession(URLSession.shared, didReceive: challenge) { dis, _ in
            disposition = dis; done.fulfill()
        }
        wait(for: [done], timeout: 2)
        XCTAssertEqual(disposition, .performDefaultHandling)
    }
}
