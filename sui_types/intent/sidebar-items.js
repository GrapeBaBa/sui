window.SIDEBAR_ITEMS = {"enum":[["AppId","This enums specifies the application ID. Two intents in two different applications (i.e., Narwhal, Sui, Ethereum etc) should never collide, so that even when a signing key is reused, nobody can take a signature designated for app_1 and present it as a valid signature for an (any) intent in app_2."],["IntentScope",""],["IntentVersion","The version here is to distinguish between signing different versions of the struct or enum. Serialized output between two different versions of the same struct/enum might accidentally (or maliciously on purpose) match."]],"struct":[["Intent","An intent is a compact struct serves as the domain separator for a message that a signature commits to. It consists of three parts: [enum IntentScope] (what the type of the message is), [enum IntentVersion], [enum AppId] (what application that the signature refers to). It is used to construct [struct IntentMessage] that what a signature commits to."],["IntentMessage","Intent Message is a wrapper around a message with its intent. The message can be any type that implements [trait Serialize]. ALL signatures in Sui must commits to the intent message, not the message itself. This guarantees any intent message signed in the system cannot collide with another since they are domain separated by intent."],["PersonalMessage","A person message that wraps around a byte array."]],"trait":[["SecureIntent",""]]};