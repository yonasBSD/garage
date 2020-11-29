pub fn xml_escape(s: &str) -> String {
	s.replace("<", "&lt;")
		.replace(">", "&gt;")
		.replace("\"", "&quot;")
}

pub fn uri_encode(string: &str, encode_slash: bool) -> String {
	let mut result = String::with_capacity(string.len() * 2);
	for c in string.chars() {
		match c {
			'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '~' | '.' => result.push(c),
			'/' if encode_slash => result.push_str("%2F"),
			'/' if !encode_slash => result.push('/'),
			_ => {
				result.push_str(
					&format!("{}", c)
						.bytes()
						.map(|b| format!("%{:02X}", b))
						.collect::<String>(),
				);
			}
		}
	}
	result
}

pub fn xml_encode_key(k: &str, urlencode: bool) -> String {
	if urlencode {
		uri_encode(k, true)
	} else {
		xml_escape(k)
	}
}