Ext.define('PBS.form.NamespaceMaxDepth', {
    extend: 'Proxmox.form.field.Integer',
    alias: 'widget.pbsNamespaceMaxDepth',

    allowBlank: true,

    emptyText: gettext('Full'),
    fieldLabel: gettext('Max. Depth'),
    deleteEmpty: true,

    minValue: 0,
    maxValue: 7,

    triggers: {
	clear: {
	    cls: 'pmx-clear-trigger',
	    weight: -1,
	    hidden: true,
	    handler: function() {
		this.triggers.clear.setVisible(false);
		this.setValue('');
	    },
	},
    },

    listeners: {
	change: function(field, value) {
	    let canClear = value !== '';
	    field.triggers.clear.setVisible(canClear);
	},
    },
});

