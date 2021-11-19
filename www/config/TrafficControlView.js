Ext.define('pmx-traffic-control', {
    extend: 'Ext.data.Model',
    fields: [
	'name', 'rate-in', 'rate-out', 'burst-in', 'burst-out', 'network',
	'timeframe', 'comment', 'cur-rate-in', 'cur-rate-out',
	{
	    name: 'rateInUsed',
	    calculate: function(data) {
		return (data['cur-rate-in'] || 0) / (data['rate-in'] || Infinity);
	    },
	},
	{
	    name: 'rateOutUsed',
	    calculate: function(data) {
		return (data['cur-rate-out'] || 0) / (data['rate-out'] || Infinity);
	    },
	},
    ],
    idProperty: 'name',
    proxy: {
	type: 'proxmox',
	url: '/api2/json/admin/traffic-control',
    },
});

Ext.define('PBS.config.TrafficControlView', {
    extend: 'Ext.grid.GridPanel',
    alias: 'widget.pbsTrafficControlView',

    stateful: true,
    stateId: 'grid-traffic-control',

    title: gettext('Traffic Control'),

//    tools: [PBS.Utils.get_help_tool("backup-remote")],

    controller: {
	xclass: 'Ext.app.ViewController',

	addRemote: function() {
	    let me = this;
            Ext.create('PBS.window.TrafficControlEdit', {
		listeners: {
		    destroy: function() {
			me.reload();
		    },
		},
            }).show();
	},

	editRemote: function() {
	    let me = this;
	    let view = me.getView();
	    let selection = view.getSelection();
	    if (selection.length < 1) return;

            Ext.create('PBS.window.TrafficControlEdit', {
                name: selection[0].data.name,
		listeners: {
		    destroy: function() {
			me.reload();
		    },
		},
            }).show();
	},

	render_bandwidth: (value) => value ? Proxmox.Utils.format_size(value) + '/s' : '',

	reload: function() { this.getView().getStore().rstore.load(); },

	init: function(view) {
	    Proxmox.Utils.monStoreErrors(view, view.getStore().rstore);
	},
    },

    listeners: {
	activate: 'reload',
	itemdblclick: 'editRemote',
    },

    store: {
	type: 'diff',
	autoDestroy: true,
	autoDestroyRstore: true,
	sorters: 'name',
	rstore: {
	    type: 'update',
	    storeid: 'pmx-traffic-control',
	    model: 'pmx-traffic-control',
	    autoStart: true,
	    interval: 5000,
	},
    },

    tbar: [
	{
	    xtype: 'proxmoxButton',
	    text: gettext('Add'),
	    handler: 'addRemote',
	    selModel: false,
	},
	{
	    xtype: 'proxmoxButton',
	    text: gettext('Edit'),
	    handler: 'editRemote',
	    disabled: true,
	},
	{
	    xtype: 'proxmoxStdRemoveButton',
	    baseurl: '/config/traffic-control',
	    callback: 'reload',
	},
    ],

    viewConfig: {
	trackOver: false,
    },

    columns: [
	{
	    header: gettext('Rule'),
	    width: 200,
	    sortable: true,
	    renderer: Ext.String.htmlEncode,
	    dataIndex: 'name',
	},
	{
	    header: gettext('Rate In'),
	    width: 200,
	    sortable: true,
	    renderer: 'render_bandwidth',
	    dataIndex: 'rate-in',
	},
	{
	    header: gettext('Rate In Used'),
	    xtype: 'widgetcolumn',
	    dataIndex: 'rateInUsed',
	    widget: {
		xtype: 'progressbarwidget',
		textTpl: '{percent:number("0")}%',
		animate: true,
	    },
	},
	{
	    header: gettext('Rate Out'),
	    width: 200,
	    sortable: true,
	    renderer: 'render_bandwidth',
	    dataIndex: 'rate-out',
	},
	{
	    header: gettext('Rate Out Used'),
	    xtype: 'widgetcolumn',
	    dataIndex: 'rateOutUsed',
	    widget: {
		xtype: 'progressbarwidget',
		textTpl: '{percent:number("0")}%',
		animate: true,
	    },
	},
	{
	    header: gettext('Burst In'),
	    width: 200,
	    sortable: true,
	    renderer: 'render_bandwidth',
	    dataIndex: 'burst-in',
	},
	{
	    header: gettext('Burst Out'),
	    width: 200,
	    sortable: true,
	    renderer: 'render_bandwidth',
	    dataIndex: 'burst-out',
	},
	{
	    header: gettext('Networks'),
	    width: 200,
	    sortable: true,
	    renderer: Ext.String.htmlEncode,
	    dataIndex: 'network',
	},
	{
	    header: gettext('Timeframes'),
	    sortable: false,
	    renderer: (timeframes) => Ext.String.htmlEncode(timeframes.join('; ')),
	    dataIndex: 'timeframe',
	    width: 200,
	},
	{
	    header: gettext('Comment'),
	    sortable: false,
	    renderer: Ext.String.htmlEncode,
	    dataIndex: 'comment',
	    flex: 1,
	},
    ],
});
