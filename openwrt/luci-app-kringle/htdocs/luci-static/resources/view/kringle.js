'use strict';
'require view';
'require form';
'require rpc';
'require uci';

var callServiceList = rpc.declare({
	object: 'service',
	method: 'list',
	params: [ 'name' ],
	expect: { '': {} }
});

function isRunning() {
	return L.resolveDefault(callServiceList('kringle'), {}).then(function(res) {
		var instances = (res && res.kringle && res.kringle.instances) || {};
		return Object.keys(instances).some(function(k) { return instances[k].running; });
	});
}

return view.extend({
	load: function() {
		return Promise.all([ uci.load('kringle'), isRunning() ]);
	},

	render: function(data) {
		var running = data[1];
		var m, s, o;

		m = new form.Map('kringle', _('Kringle'),
			_('A Kris Kringle / Secret Santa gift swap. People just open the address in a browser: no app, no accounts. Save & Apply restarts Kringle with the new settings.'));

		s = m.section(form.NamedSection, 'main', 'kringle', _('Settings'));
		s.addremove = false;

		o = s.option(form.DummyValue, '_status', _('Status'));
		o.rawhtml = true;
		o.cfgvalue = function() {
			var port = parseInt(uci.get('kringle', 'main', 'port'), 10) || 8787;
			var url = 'http://' + window.location.hostname + ':' + port + '/';
			if (!running)
				return '<em>' + _('Not running') + '</em>';
			return '<strong>' + _('Running') + '</strong> &middot; <a href="' + url + '" target="_blank" rel="noreferrer">' + _('Open Kringle') + '</a>';
		};

		o = s.option(form.Flag, 'enabled', _('Enabled'));
		o.default = '1';
		o.rmempty = false;

		o = s.option(form.Value, 'port', _('Port'),
			_('TCP port Kringle listens on. Forward this port if people outside your network should reach it.'));
		o.datatype = 'port';
		o.placeholder = '8787';
		o.rmempty = false;

		o = s.option(form.ListValue, 'listen', _('Listen on'));
		o.value('all', _('All interfaces'));
		o.value('lan', _('LAN only'));
		o.default = 'all';

		o = s.option(form.Value, 'public_url', _('Public address'),
			_('The address used in the invite and personal links Kringle hands out, for example <code>http://gifts.example.com:8787</code>. Set it when you forward a port so the links work from outside. Leave empty to use whatever address each browser used.'));
		o.placeholder = 'http://gifts.example.com:8787';
		o.validate = function(section_id, value) {
			if (!value)
				return true;
			return /^https?:\/\/[^\s\/?#]+\/?$/.test(value) ? true : _('Use http:// or https:// followed by a host name and optional port, with no path.');
		};

		o = s.option(form.Value, 'keep_days', _('Delete groups after (days)'),
			_('Groups and everyone’s wishes are deleted this many days after the group was created.'));
		o.datatype = 'range(1,3650)';
		o.placeholder = '120';

		o = s.option(form.Value, 'data_dir', _('Data folder'),
			_('Where the data file is kept. Must be on persistent storage, not /tmp. It is kept across firmware upgrades if left at the default.'));
		o.placeholder = '/etc/kringle';
		o.validate = function(section_id, value) {
			if (!value || /^\/[^\s]*$/.test(value))
				return true;
			return _('Use an absolute path such as /etc/kringle.');
		};

		return m.render();
	}
});
