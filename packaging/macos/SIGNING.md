# Signing the macOS app

`bundle.sh` signs `Strata.app` ad hoc unless it is given an identity. An ad-hoc
signature is computed over the binary, so macOS sees every release as a
different app: the Keychain forgets "Always Allow" choices and privacy grants
are asked for again after each update. Signing with a certificate gives the app
a designated requirement built from the certificate and bundle id instead, and
that requirement does not change between releases.

The certificate is self-signed, so this needs no Apple Developer Program
membership and no notarisation. Gatekeeper still warns about downloaded copies,
and the first launch still needs the Finder's "Open" bypass.

## Create the certificate

The identity is `Strata Code Signing`, which is the common name of the
certificate. Either route works; long validity is what matters, because a
replacement certificate is a new identity to macOS.

Keychain Access: Certificate Assistant → Create a Certificate…, name
`Strata Code Signing`, identity type "Self Signed Root", certificate type "Code
Signing", 3650 days, then export the certificate and its private key as a
`.p12` with a password.

Or with `openssl`:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout StrataSigning.key -out StrataSigning.crt \
  -subj "/CN=Strata Code Signing" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning"
openssl pkcs12 -export -inkey StrataSigning.key -in StrataSigning.crt \
  -out StrataSigning.p12 -name "Strata Code Signing"
```

OpenSSL 3 and later default to a PKCS#12 encryption that macOS refuses with
`MAC verification failed during PKCS12 import`; add `-legacy` to the `pkcs12`
command there. The `/usr/bin/openssl` that ships with macOS needs no flag.

Keep `StrataSigning.p12` and the password out of the repository; commit
neither.

## Sign locally

Import the archive into a keychain that `codesign` searches, for example by
opening the `.p12` in Keychain Access and allowing access to the key. Then:

```sh
direnv exec . packaging/macos/bundle.sh --sign "Strata Code Signing"
codesign -dr - dist/Strata.app
```

The designated requirement should name the certificate, not a hash of the
binary:

```
designated => identifier "dev.strata.Strata" and certificate leaf = H"…"
```

`security find-identity -v -p codesigning` does not list the certificate, since
the system does not trust a self-signed one; drop `-v` to see it.

## Register the secrets

The release workflow imports a temporary keychain from two repository secrets
and passes the identity to `bundle.sh`. Without them it logs a warning and
falls back to the ad-hoc signature.

```sh
base64 < StrataSigning.p12 | tr -d '\n' | gh secret set STRATA_SIGNING_P12
gh secret set STRATA_SIGNING_P12_PASSWORD
```

The first command pipes the base64 of the archive; the second prompts for the
password so it never appears in the shell history. Once set, the next release
prints the designated requirement in its "Verify the app signature" step.
